// ! Stats aggregation for batching client stats payloads.
//!
//! This module provides batching logic for `ClientStatsPayload` objects before
//! forwarding them to Datadog. It aggregates stats from both client-computed stats
//! and agent-generated stats into size-limited batches.
//!
//! # Purpose
//!
//! The stats aggregator serves two key functions:
//! 1. **Batching**: Combines multiple small stats payloads into batches
//! 2. **Size Limiting**: Ensures batches don't exceed maximum payload size (~3MB)
//!
//! # Architecture
//!
//! ```text
//! Stats Sources → StatsAggregator → Size-Limited Batches → StatsFlusher → Datadog
//!                       ↓
//!                 FIFO Queue (5,000 payloads)
//! ```
//!
//! # Integration
//!
//! The aggregator integrates with the stats concentrator to pull agent-generated
//! stats during batch retrieval, combining them with client-computed stats.
//!
//! # Memory Management
//!
//! With a limit of 5,000 payloads and typical payload sizes:
//! - Small payloads (~500 bytes): ~2.5MB buffered
//! - Large payloads (~3KB): ~15MB buffered
//! - FIFO eviction prevents unbounded growth

use crate::traces::stats_concentrator_service::StatsConcentratorHandle;
use datadog_trace_protobuf::pb::ClientStatsPayload;
use std::collections::VecDeque;
use tracing::{error, warn};

#[allow(clippy::empty_line_after_doc_comments)]
/// Maximum number of entries in a stat payload.
///
/// <https://github.com/DataDog/datadog-agent/blob/996dd54337908a6511948fabd2a41420ba919a8b/pkg/trace/writer/stats.go#L35-L41>
// const MAX_BATCH_ENTRIES_SIZE: usize = 4000;

// Aproximate size an entry in a stat payload occupies
//
// <https://github.com/DataDog/datadog-agent/blob/996dd54337908a6511948fabd2a41420ba919a8b/pkg/trace/writer/stats.go#L33-L35>
// const MAX_ENTRY_SIZE_BYTES: usize = 375;

/// Maximum content size per payload in compressed bytes.
///
/// This limit is set to ~3MB to match the Datadog agent's behavior and ensure
/// payloads can be processed efficiently by the backend without hitting size limits.
///
/// Reference: <https://github.com/DataDog/datadog-agent/blob/996dd54337908a6511948fabd2a41420ba919a8b/pkg/trace/writer/stats.go#L35-L41>
const MAX_CONTENT_SIZE_BYTES: usize = 3 * 1024 * 1024; // ~3MB

/// Maximum number of stats payloads that can be queued before eviction.
///
/// This prevents unbounded memory growth under high load by implementing
/// FIFO (First-In-First-Out) eviction when the queue is full.
///
/// # Memory Calculation
///
/// Assuming average payload size of ~3KB:
/// - `5,000 payloads × 3KB = ~15MB` of buffered stats
///
/// Adjust based on memory constraints and traffic patterns.
const MAX_STATS_QUEUE_ITEMS: usize = 5_000;

/// Aggregator for batching client stats payloads before flushing.
///
/// The `StatsAggregator` buffers individual `ClientStatsPayload` objects and
/// combines them into size-limited batches for efficient delivery to Datadog.
/// It integrates with the stats concentrator to pull agent-generated stats.
///
/// # Batching Strategy
///
/// - Payloads are queued in FIFO order
/// - Batches are created on demand with size limit (~3MB)
/// - If queue is full (5,000 items), oldest payloads are evicted
///
/// # Usage
///
/// ```rust
/// use datadog_agent_native::traces::stats_aggregator::StatsAggregator;
/// // let concentrator_handle = ...; // From stats concentrator service
/// // let mut aggregator = StatsAggregator::new_with_concentrator(concentrator_handle);
/// //
/// // // Add individual payloads
/// // aggregator.add(payload);
/// //
/// // // Get size-limited batch for flushing
/// // let batch = aggregator.get_batch(false).await;
/// ```
#[allow(clippy::module_name_repetitions)]
pub struct StatsAggregator {
    /// FIFO queue of stats payloads waiting to be batched.
    queue: VecDeque<ClientStatsPayload>,
    /// Maximum batch size in bytes (compressed).
    max_content_size_bytes: usize,
    /// Maximum number of queued payloads before eviction.
    max_queue_items: usize,
    /// Temporary buffer for building batches (reused to avoid allocations).
    buffer: Vec<ClientStatsPayload>,
    /// Handle to stats concentrator for pulling agent-generated stats.
    concentrator: StatsConcentratorHandle,
}

impl StatsAggregator {
    /// Creates a new stats aggregator with custom size limit.
    ///
    /// # Arguments
    ///
    /// * `max_content_size_bytes` - Maximum batch size in compressed bytes
    /// * `concentrator` - Handle to stats concentrator for pulling agent-generated stats
    ///
    /// # Returns
    ///
    /// A new aggregator instance with empty queue and buffer.
    #[allow(dead_code)]
    #[allow(clippy::must_use_candidate)]
    fn new(max_content_size_bytes: usize, concentrator: StatsConcentratorHandle) -> Self {
        StatsAggregator {
            queue: VecDeque::new(),
            max_content_size_bytes,
            max_queue_items: MAX_STATS_QUEUE_ITEMS,
            buffer: Vec::new(),
            concentrator,
        }
    }

    /// Creates a new stats aggregator with default size limit (~3MB).
    ///
    /// This is the primary constructor for creating a stats aggregator with
    /// standard batch size limits matching the Datadog agent behavior.
    ///
    /// # Arguments
    ///
    /// * `concentrator` - Handle to stats concentrator for pulling agent-generated stats
    ///
    /// # Returns
    ///
    /// A new aggregator instance configured with:
    /// - Max content size: 3MB (`MAX_CONTENT_SIZE_BYTES`)
    /// - Max queue items: 5,000 (`MAX_STATS_QUEUE_ITEMS`)
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::stats_aggregator::StatsAggregator;
    /// use datadog_agent_native::traces::stats_concentrator_service::StatsConcentratorService;
    /// use datadog_agent_native::config::Config;
    /// use std::sync::Arc;
    ///
    /// # async fn example() {
    /// let config = Arc::new(Config::default());
    /// let (service, concentrator_handle) = StatsConcentratorService::new(config);
    /// let aggregator = StatsAggregator::new_with_concentrator(concentrator_handle);
    /// # }
    /// ```
    #[must_use]
    pub fn new_with_concentrator(concentrator: StatsConcentratorHandle) -> Self {
        Self::new(MAX_CONTENT_SIZE_BYTES, concentrator)
    }

    /// Adds an individual stats payload to the aggregation queue.
    ///
    /// If the queue is at capacity (`MAX_STATS_QUEUE_ITEMS`), the oldest payload
    /// is evicted (dropped) to prevent unbounded memory growth. This implements
    /// a FIFO eviction policy for backpressure under high load.
    ///
    /// # Arguments
    ///
    /// * `payload` - Client stats payload to buffer for batching
    ///
    /// # Eviction Policy
    ///
    /// When the queue is full (5,000 items):
    /// 1. The oldest payload (front of queue) is removed and dropped
    /// 2. A warning is logged with estimated payload size
    /// 3. The new payload is added to the back of the queue
    ///
    /// # Performance
    ///
    /// - Add: O(1) amortized (VecDeque push_back)
    /// - Evict: O(1) (VecDeque pop_front)
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::stats_aggregator::StatsAggregator;
    /// use datadog_trace_protobuf::pb::ClientStatsPayload;
    ///
    /// // let mut aggregator = ...; // From new_with_concentrator
    /// // let payload = ClientStatsPayload { /* ... */ };
    /// // aggregator.add(payload);
    /// ```
    pub fn add(&mut self, payload: ClientStatsPayload) {
        // Check if queue is at capacity
        if self.queue.len() >= self.max_queue_items {
            // Evict oldest payload (FIFO eviction policy)
            if let Some(evicted) = self.queue.pop_front() {
                warn!(
                    "Stats aggregator queue full ({} items), dropping oldest stats payload (estimated {} bytes)",
                    self.max_queue_items,
                    Self::estimate_payload_size(&evicted)
                );
            }
        }
        self.queue.push_back(payload);
    }

    /// Calculates approximate payload size including all heap-allocated data.
    ///
    /// This method provides a more accurate size estimate than `size_of_val`,
    /// which only returns stack size. It accounts for:
    /// - All heap-allocated strings (hostname, env, version, etc.)
    /// - Tags vector elements
    /// - Stats buckets (estimated at ~500 bytes each)
    ///
    /// # Arguments
    ///
    /// * `payload` - Payload to estimate size for
    ///
    /// # Returns
    ///
    /// Estimated payload size in bytes (uncompressed).
    ///
    /// # Accuracy
    ///
    /// The estimate is conservative and may overestimate actual compressed size,
    /// but provides good batching behavior for size-limited payloads.
    fn estimate_payload_size(payload: &ClientStatsPayload) -> usize {
        let mut size = std::mem::size_of::<ClientStatsPayload>();

        // Add sizes of heap-allocated strings
        size += payload.hostname.len();
        size += payload.env.len();
        size += payload.version.len();
        size += payload.lang.len();
        size += payload.tracer_version.len();
        size += payload.runtime_id.len();
        size += payload.agent_aggregation.len();
        size += payload.service.len();
        size += payload.container_id.len();
        size += payload.git_commit_sha.len();
        size += payload.image_tag.len();
        size += payload.process_tags.len();

        // Add size of tags vector
        for tag in &payload.tags {
            size += tag.len();
        }

        // Add approximate size of stats buckets
        // Each stat bucket has various fields - use a conservative estimate
        size += payload.stats.len() * 500; // ~500 bytes per stats bucket

        size
    }

    /// Returns a batch of stats payloads, subject to max content size.
    ///
    /// This method:
    /// 1. Flushes the stats concentrator to get agent-generated stats
    /// 2. Combines client-computed and agent-generated stats
    /// 3. Builds a size-limited batch (up to ~3MB)
    /// 4. Returns the batch for delivery to Datadog
    ///
    /// # Arguments
    ///
    /// * `force_flush` - If `true`, force-flush the concentrator (all buckets).
    ///                   If `false`, only completed buckets.
    ///
    /// # Returns
    ///
    /// A vector of `ClientStatsPayload` objects representing one batch.
    /// The batch size is limited by `max_content_size_bytes` (~3MB).
    ///
    /// # Batching Algorithm
    ///
    /// 1. Pull agent-generated stats from concentrator and add to queue
    /// 2. Iterate through queue in FIFO order
    /// 3. Add payloads to batch until size limit would be exceeded
    /// 4. If a payload doesn't fit, push it back to front of queue
    /// 5. Return the batch (empties internal buffer)
    ///
    /// # Error Handling
    ///
    /// If the concentrator flush fails, logs an error and continues with
    /// existing queued payloads (graceful degradation).
    ///
    /// # Performance
    ///
    /// - Reuses internal buffer to avoid allocations
    /// - O(n) where n is the number of payloads that fit in the batch
    /// - Typical batch: 50-200 payloads depending on size
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::stats_aggregator::StatsAggregator;
    ///
    /// # async fn example(mut aggregator: StatsAggregator) {
    /// // Get a batch for normal flush (only completed buckets)
    /// let batch = aggregator.get_batch(false).await;
    /// println!("Flushing {} stats payloads", batch.len());
    ///
    /// // During shutdown, use force flush to avoid losing recent stats
    /// let final_batch = aggregator.get_batch(true).await;
    /// # }
    /// ```
    pub async fn get_batch(&mut self, force_flush: bool) -> Vec<ClientStatsPayload> {
        // Pull stats data from concentrator and add to queue
        match self.concentrator.flush(force_flush).await {
            Ok(stats) => {
                if let Some(stats) = stats {
                    // Add agent-generated stats to the queue
                    self.queue.push_back(stats);
                }
            }
            Err(e) => {
                // Log error but continue with existing queued payloads (graceful degradation)
                error!("Error getting stats from the stats concentrator: {e:?}");
            }
        }

        let mut batch_size = 0;

        // Fill the batch up to max content size
        while batch_size < self.max_content_size_bytes {
            if let Some(payload) = self.queue.pop_front() {
                let payload_size = Self::estimate_payload_size(&payload);

                // Check if adding this payload would exceed the size limit
                if batch_size + payload_size > self.max_content_size_bytes {
                    // Payload doesn't fit - put it back at the front for next batch
                    self.queue.push_front(payload);
                    break;
                }

                // Payload fits - add to batch
                batch_size += payload_size;
                self.buffer.push(payload);
            } else {
                // Queue is empty - batch is complete
                break;
            }
        }

        // Return the batch and clear the buffer for next batch
        std::mem::take(&mut self.buffer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::traces::stats_concentrator_service::StatsConcentratorService;
    use std::sync::Arc;

    #[test]
    fn test_add() {
        let config = Arc::new(Config::default());
        let (_, concentrator) = StatsConcentratorService::new(config);
        let mut aggregator = StatsAggregator::new_with_concentrator(concentrator);
        let payload = ClientStatsPayload {
            hostname: "hostname".to_string(),
            env: "dev".to_string(),
            version: "version".to_string(),
            stats: vec![],
            lang: "rust".to_string(),
            tracer_version: "tracer.version".to_string(),
            runtime_id: "hash".to_string(),
            sequence: 0,
            agent_aggregation: "aggregation".to_string(),
            service: "service".to_string(),
            container_id: "container_id".to_string(),
            tags: vec![],
            git_commit_sha: "git_commit_sha".to_string(),
            image_tag: "image_tag".to_string(),
            process_tags: "process_tags".to_string(),
            process_tags_hash: 0,
        };

        aggregator.add(payload.clone());
        assert_eq!(aggregator.queue.len(), 1);
        assert_eq!(aggregator.queue[0], payload);
    }

    #[tokio::test]
    async fn test_get_batch() {
        let config = Arc::new(Config::default());
        let (_, concentrator) = StatsConcentratorService::new(config);
        let mut aggregator = StatsAggregator::new_with_concentrator(concentrator);
        let payload = ClientStatsPayload {
            hostname: "hostname".to_string(),
            env: "dev".to_string(),
            version: "version".to_string(),
            stats: vec![],
            lang: "rust".to_string(),
            tracer_version: "tracer.version".to_string(),
            runtime_id: "hash".to_string(),
            sequence: 0,
            agent_aggregation: "aggregation".to_string(),
            service: "service".to_string(),
            container_id: "container_id".to_string(),
            tags: vec![],
            git_commit_sha: "git_commit_sha".to_string(),
            image_tag: "image_tag".to_string(),
            process_tags: "process_tags".to_string(),
            process_tags_hash: 0,
        };
        aggregator.add(payload.clone());
        assert_eq!(aggregator.queue.len(), 1);
        let batch = aggregator.get_batch(false).await;
        assert_eq!(batch, vec![payload]);
    }

    #[tokio::test]
    async fn test_get_batch_full_entries() {
        let config = Arc::new(Config::default());
        let (_, concentrator) = StatsConcentratorService::new(config);
        // With the new accurate size estimation, each payload is ~530 bytes (including string data)
        // Set limit to fit 2 payloads but not 3
        let mut aggregator = StatsAggregator::new(1100, concentrator);
        let payload = ClientStatsPayload {
            hostname: "hostname".to_string(),
            env: "dev".to_string(),
            version: "version".to_string(),
            stats: vec![],
            lang: "rust".to_string(),
            tracer_version: "tracer.version".to_string(),
            runtime_id: "hash".to_string(),
            sequence: 0,
            agent_aggregation: "aggregation".to_string(),
            service: "service".to_string(),
            container_id: "container_id".to_string(),
            tags: vec![],
            git_commit_sha: "git_commit_sha".to_string(),
            image_tag: "image_tag".to_string(),
            process_tags: "process_tags".to_string(),
            process_tags_hash: 0,
        };

        // Add 3 payloads
        aggregator.add(payload.clone());
        aggregator.add(payload.clone());
        aggregator.add(payload.clone());

        // The batch should only contain the first 2 payloads
        let first_batch = aggregator.get_batch(false).await;
        assert_eq!(first_batch, vec![payload.clone(), payload.clone()]);
        assert_eq!(aggregator.queue.len(), 1);

        // The second batch should only contain the last log
        let second_batch = aggregator.get_batch(false).await;
        assert_eq!(second_batch, vec![payload]);
        assert_eq!(aggregator.queue.len(), 0);
    }
}
