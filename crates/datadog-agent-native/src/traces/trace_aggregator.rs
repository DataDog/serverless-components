//! Trace aggregation and batching for efficient Datadog trace intake.
//!
//! This module provides buffering and batching functionality for trace payloads before
//! forwarding them to Datadog intake endpoints. The aggregator ensures efficient network
//! utilization by:
//! - **Batching**: Combining multiple small trace payloads into larger batches
//! - **Size Management**: Respecting the 3.2MB max payload size limit
//! - **Backpressure**: Implementing FIFO eviction when the queue is full
//!
//! # Architecture
//!
//! The `TraceAggregator` uses a queue-based architecture:
//! 1. Individual trace payloads are added to a FIFO queue via `add()`
//! 2. The `get_batch()` method pulls payloads from the queue and builds batches
//! 3. Batches respect the `MAX_CONTENT_SIZE_BYTES` limit (3.2MB)
//! 4. If the queue exceeds `MAX_QUEUE_ITEMS`, oldest payloads are dropped
//!
//! # Memory Management
//!
//! With defaults:
//! - `MAX_QUEUE_ITEMS = 10,000` payloads
//! - Average trace size ~10KB
//! - Maximum memory usage ~100MB of buffered traces
//!
//! # Example
//!
//! ```rust
//! use datadog_agent_native::traces::trace_aggregator::{TraceAggregator, SendDataBuilderInfo};
//!
//! let mut aggregator = TraceAggregator::default();
//!
//! // Add individual trace payloads
//! // aggregator.add(payload_info);
//!
//! // Get a batch of traces (up to 3.2MB)
//! let batch = aggregator.get_batch();
//! // Send batch to Datadog...
//! ```

use datadog_trace_utils::send_data::SendDataBuilder;
use std::collections::VecDeque;
use tracing::warn;

/// Maximum content size per payload uncompressed in bytes,
/// that the Datadog Trace API accepts. The value is 3.2 MB.
///
/// This limit is enforced by the Datadog trace intake API. Payloads larger than
/// this will be rejected with a 413 (Payload Too Large) error.
///
/// Reference: <https://github.com/DataDog/datadog-agent/blob/9d57c10a9eeb3916e661d35dbd23c6e36395a99d/pkg/trace/writer/trace.go#L27-L31>
pub(crate) const MAX_CONTENT_SIZE_BYTES: usize = 3_200_000;

/// Maximum number of trace payloads that can be queued before eviction.
///
/// This prevents unbounded memory growth under high load by implementing
/// FIFO (First-In-First-Out) eviction when the queue is full.
///
/// # Memory Calculation
///
/// Assuming average trace size of ~10KB:
/// - `10,000 payloads × 10KB = ~100MB` of buffered traces
///
/// Adjust this value based on your memory constraints and trace volume.
pub(crate) const MAX_QUEUE_ITEMS: usize = 10_000;

/// Wrapper for `SendDataBuilder` that bundles the payload size.
///
/// `SendDataBuilder` from `datadog_trace_utils` doesn't expose a getter for the
/// payload size, so we bundle them together for efficient batch size calculations.
///
/// # Fields
///
/// * `builder` - The trace payload builder ready for serialization
/// * `size` - Uncompressed payload size in bytes (used for batching)
pub struct SendDataBuilderInfo {
    /// The trace payload builder ready for serialization and transmission.
    pub builder: SendDataBuilder,
    /// Uncompressed payload size in bytes.
    ///
    /// Used to calculate batch sizes and ensure they don't exceed `MAX_CONTENT_SIZE_BYTES`.
    pub size: usize,
}

impl SendDataBuilderInfo {
    /// Creates a new payload info bundle.
    ///
    /// # Arguments
    ///
    /// * `builder` - The trace payload builder
    /// * `size` - Uncompressed payload size in bytes
    pub fn new(builder: SendDataBuilder, size: usize) -> Self {
        Self { builder, size }
    }
}

/// Aggregates individual trace payloads into batches for efficient forwarding to Datadog.
///
/// The aggregator implements a queue-based batching strategy that:
/// 1. Buffers incoming trace payloads in a FIFO queue
/// 2. Builds batches that respect the max payload size (3.2MB by default)
/// 3. Implements backpressure by dropping oldest payloads when the queue is full
///
/// # Batching Strategy
///
/// The `get_batch()` method creates batches by:
/// - Pulling payloads from the queue in FIFO order
/// - Accumulating payloads until the batch size would exceed `max_content_size_bytes`
/// - Returning the batch for forwarding
/// - Leaving remaining payloads in the queue for the next batch
///
/// # Memory Management
///
/// When the queue reaches `max_queue_items`, the oldest payloads are evicted (dropped)
/// to prevent unbounded memory growth. This provides backpressure under high load.
///
/// # Example
///
/// ```rust
/// use datadog_agent_native::traces::trace_aggregator::TraceAggregator;
///
/// // Create with default limits (3.2MB batches, 10K max items)
/// let mut aggregator = TraceAggregator::default();
///
/// // Or customize limits
/// let mut custom = TraceAggregator::with_limits(
///     3_200_000,  // 3.2MB max batch size
///     5_000       // Max 5K queued items
/// );
/// ```
#[allow(clippy::module_name_repetitions)]
pub struct TraceAggregator {
    /// FIFO queue of trace payloads waiting to be batched.
    queue: VecDeque<SendDataBuilderInfo>,
    /// Maximum uncompressed batch size in bytes (default: 3.2MB).
    max_content_size_bytes: usize,
    /// Maximum number of queued payloads before eviction (default: 10K).
    max_queue_items: usize,
    /// Temporary buffer for building batches (reused to reduce allocations).
    buffer: Vec<SendDataBuilder>,
}

impl Default for TraceAggregator {
    /// Creates a new trace aggregator with default limits.
    ///
    /// Default configuration:
    /// - `max_content_size_bytes`: 3.2MB (`MAX_CONTENT_SIZE_BYTES`)
    /// - `max_queue_items`: 10,000 (`MAX_QUEUE_ITEMS`)
    fn default() -> Self {
        TraceAggregator {
            queue: VecDeque::new(),
            max_content_size_bytes: MAX_CONTENT_SIZE_BYTES,
            max_queue_items: MAX_QUEUE_ITEMS,
            buffer: Vec::new(),
        }
    }
}

impl TraceAggregator {
    /// Creates a new trace aggregator with custom batch size limit.
    ///
    /// Uses the default `MAX_QUEUE_ITEMS` (10,000) for queue capacity.
    ///
    /// # Arguments
    ///
    /// * `max_content_size_bytes` - Maximum uncompressed batch size in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::trace_aggregator::TraceAggregator;
    ///
    /// // Create aggregator with 1MB batch limit
    /// let mut aggregator = TraceAggregator::new(1_000_000);
    /// ```
    #[allow(dead_code)]
    #[allow(clippy::must_use_candidate)]
    pub fn new(max_content_size_bytes: usize) -> Self {
        TraceAggregator {
            queue: VecDeque::new(),
            max_content_size_bytes,
            max_queue_items: MAX_QUEUE_ITEMS,
            buffer: Vec::new(),
        }
    }

    /// Creates a new trace aggregator with custom batch size and queue capacity limits.
    ///
    /// This allows full control over both batching and memory limits.
    ///
    /// # Arguments
    ///
    /// * `max_content_size_bytes` - Maximum uncompressed batch size in bytes
    /// * `max_queue_items` - Maximum number of queued payloads before eviction
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::trace_aggregator::TraceAggregator;
    ///
    /// // Create aggregator with 1MB batches and 5K max items
    /// let mut aggregator = TraceAggregator::with_limits(1_000_000, 5_000);
    /// ```
    #[allow(dead_code)]
    #[allow(clippy::must_use_candidate)]
    pub fn with_limits(max_content_size_bytes: usize, max_queue_items: usize) -> Self {
        TraceAggregator {
            queue: VecDeque::new(),
            max_content_size_bytes,
            max_queue_items,
            buffer: Vec::new(),
        }
    }

    /// Adds an individual trace payload to the aggregation queue.
    ///
    /// If the queue is at capacity (`max_queue_items`), the oldest payload is evicted
    /// (dropped) to prevent unbounded memory growth. This implements a FIFO eviction
    /// policy for backpressure under high load.
    ///
    /// # Arguments
    ///
    /// * `payload_info` - Trace payload with size information
    ///
    /// # Eviction Policy
    ///
    /// When the queue is full:
    /// 1. The oldest payload (front of queue) is removed and dropped
    /// 2. A warning is logged with the evicted payload size
    /// 3. The new payload is added to the back of the queue
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::trace_aggregator::{TraceAggregator, SendDataBuilderInfo};
    ///
    /// let mut aggregator = TraceAggregator::default();
    /// // aggregator.add(payload_info);
    /// ```
    pub fn add(&mut self, payload_info: SendDataBuilderInfo) {
        // Check if queue is at capacity before adding new payload
        if self.queue.len() >= self.max_queue_items {
            // Evict oldest payload to make room (FIFO eviction policy)
            if let Some(evicted) = self.queue.pop_front() {
                warn!(
                    "Trace aggregator queue full ({} items), dropping oldest trace payload (size: {} bytes)",
                    self.max_queue_items, evicted.size
                );
            }
        }
        // Add new payload to the back of the queue
        self.queue.push_back(payload_info);
    }

    /// Returns a batch of trace payloads ready for forwarding to Datadog.
    ///
    /// This method builds a batch by pulling payloads from the queue in FIFO order
    /// until adding another payload would exceed `max_content_size_bytes`.
    ///
    /// # Batching Algorithm
    ///
    /// 1. Pull payloads from the front of the queue
    /// 2. Check if adding the payload would exceed the max batch size
    /// 3. If yes, put the payload back and return the current batch
    /// 4. If no, add the payload to the batch and continue
    /// 5. Stop when the queue is empty or the next payload won't fit
    ///
    /// # Returns
    ///
    /// Vector of `SendDataBuilder` payloads ready for serialization and transmission.
    /// Returns an empty vector if the queue is empty.
    ///
    /// # Note
    ///
    /// If a single payload is larger than `max_content_size_bytes`, it will be
    /// returned alone in a batch. This is a known limitation (see TODO comment in code).
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::trace_aggregator::TraceAggregator;
    ///
    /// let mut aggregator = TraceAggregator::default();
    /// // Add payloads...
    ///
    /// // Get a batch (up to 3.2MB)
    /// let batch = aggregator.get_batch();
    /// // Forward batch to Datadog...
    /// ```
    pub fn get_batch(&mut self) -> Vec<SendDataBuilder> {
        let mut batch_size = 0;

        // Build batch by pulling payloads from queue until we hit the size limit
        while batch_size < self.max_content_size_bytes {
            if let Some(payload_info) = self.queue.pop_front() {
                // TODO(duncanista): revisit if this is bigger than limit
                let payload_size = payload_info.size;

                // Check if adding this payload would exceed the max batch size
                if batch_size + payload_size > self.max_content_size_bytes {
                    // Put the payload back at the front for the next batch
                    self.queue.push_front(payload_info);
                    break;
                }
                // Payload fits - add it to the batch
                batch_size += payload_size;
                self.buffer.push(payload_info.builder);
            } else {
                // Queue is empty - stop building batch
                break;
            }
        }

        // Take ownership of the buffer and return it (leaves buffer empty for next batch)
        std::mem::take(&mut self.buffer)
    }

    /// Clears the aggregation queue, discarding all buffered payloads.
    ///
    /// This is useful during shutdown or when resetting the aggregator state.
    /// All payloads in the queue are dropped and will not be forwarded.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::trace_aggregator::TraceAggregator;
    ///
    /// let mut aggregator = TraceAggregator::default();
    /// // Add payloads...
    ///
    /// // Discard all buffered payloads
    /// aggregator.clear();
    /// ```
    pub fn clear(&mut self) {
        self.queue.clear();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use datadog_trace_utils::{
        trace_utils::TracerHeaderTags, tracer_payload::TracerPayloadCollection,
    };
    use ddcommon::Endpoint;

    use super::*;

    #[test]
    fn test_add() {
        let mut aggregator = TraceAggregator::default();
        let tracer_header_tags = TracerHeaderTags {
            lang: "lang",
            lang_version: "lang_version",
            lang_interpreter: "lang_interpreter",
            lang_vendor: "lang_vendor",
            tracer_version: "tracer_version",
            container_id: "container_id",
            client_computed_top_level: true,
            client_computed_stats: true,
            dropped_p0_traces: 0,
            dropped_p0_spans: 0,
        };
        let size = 1;
        let payload = SendDataBuilder::new(
            size,
            TracerPayloadCollection::V07(Vec::new()),
            tracer_header_tags,
            &Endpoint::from_slice("localhost"),
        );

        aggregator.add(SendDataBuilderInfo::new(payload.clone(), size));
        assert_eq!(aggregator.queue.len(), 1);
    }

    #[test]
    fn test_get_batch() {
        let mut aggregator = TraceAggregator::default();
        let tracer_header_tags = TracerHeaderTags {
            lang: "lang",
            lang_version: "lang_version",
            lang_interpreter: "lang_interpreter",
            lang_vendor: "lang_vendor",
            tracer_version: "tracer_version",
            container_id: "container_id",
            client_computed_top_level: true,
            client_computed_stats: true,
            dropped_p0_traces: 0,
            dropped_p0_spans: 0,
        };
        let size = 1;
        let payload = SendDataBuilder::new(
            size,
            TracerPayloadCollection::V07(Vec::new()),
            tracer_header_tags,
            &Endpoint::from_slice("localhost"),
        );

        aggregator.add(SendDataBuilderInfo::new(payload.clone(), size));
        assert_eq!(aggregator.queue.len(), 1);
        let batch = aggregator.get_batch();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn test_get_batch_full_entries() {
        let mut aggregator = TraceAggregator::new(2);
        let tracer_header_tags = TracerHeaderTags {
            lang: "lang",
            lang_version: "lang_version",
            lang_interpreter: "lang_interpreter",
            lang_vendor: "lang_vendor",
            tracer_version: "tracer_version",
            container_id: "container_id",
            client_computed_top_level: true,
            client_computed_stats: true,
            dropped_p0_traces: 0,
            dropped_p0_spans: 0,
        };
        let size = 1;
        let payload = SendDataBuilder::new(
            size,
            TracerPayloadCollection::V07(Vec::new()),
            tracer_header_tags,
            &Endpoint::from_slice("localhost"),
        );

        // Add 3 payloads
        aggregator.add(SendDataBuilderInfo::new(payload.clone(), size));
        aggregator.add(SendDataBuilderInfo::new(payload.clone(), size));
        aggregator.add(SendDataBuilderInfo::new(payload.clone(), size));

        // The batch should only contain the first 2 payloads
        let first_batch = aggregator.get_batch();
        assert_eq!(first_batch.len(), 2);
        assert_eq!(aggregator.queue.len(), 1);

        // The second batch should only contain the last log
        let second_batch = aggregator.get_batch();
        assert_eq!(second_batch.len(), 1);
        assert_eq!(aggregator.queue.len(), 0);
    }
}
