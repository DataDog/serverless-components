//! HTTP proxy request aggregation for Datadog intake endpoints.
//!
//! This module provides buffering for HTTP proxy requests that need to be forwarded
//! to Datadog intake endpoints. It's used when the agent acts as a proxy, receiving
//! requests from clients and forwarding them to Datadog with potential modifications.
//!
//! # Proxy Pattern
//!
//! The proxy aggregator implements a buffering pattern for HTTP requests:
//! 1. **Receive**: Client sends HTTP request to agent's proxy endpoint
//! 2. **Buffer**: Request (headers, body, URL, params) is queued
//! 3. **Batch**: Multiple requests can be retrieved at once
//! 4. **Forward**: Proxy flusher sends requests to Datadog
//!
//! # Use Cases
//!
//! - **Credential Injection**: Agent adds API key to outgoing requests
//! - **Request Modification**: Headers, URLs, or body can be transformed
//! - **Buffering**: Smooth out request spikes during high load
//! - **Retry Logic**: Failed requests can be re-queued
//!
//! # Memory Management
//!
//! With a limit of 1,000 requests and typical request sizes:
//! - Small requests (~1KB): ~1MB buffered
//! - Large requests (~10KB): ~10MB buffered
//! - Eviction prevents unbounded growth under load

use bytes::Bytes;
use reqwest::header::HeaderMap;
use std::collections::{HashMap, VecDeque};
use tracing::warn;

/// Maximum number of proxy requests that can be queued before eviction.
///
/// This prevents unbounded memory growth under high load by implementing
/// FIFO (First-In-First-Out) eviction when the queue is full.
///
/// # Memory Calculation
///
/// Assuming average request size of ~5KB:
/// - `1,000 requests × 5KB = ~5MB` of buffered requests
///
/// Adjust based on your memory constraints and traffic patterns.
const MAX_QUEUE_ITEMS: usize = 1000;

/// HTTP proxy request to be forwarded to Datadog.
///
/// Captures all components of an HTTP request that need to be proxied,
/// including headers, body, target URL, and query parameters.
pub struct ProxyRequest {
    /// HTTP headers from the original request.
    ///
    /// May include authentication, content-type, custom headers, etc.
    pub headers: HeaderMap,

    /// Request body as raw bytes.
    ///
    /// Could be JSON, msgpack, protobuf, or other formats depending
    /// on the endpoint being proxied.
    pub body: Bytes,

    /// Target URL to forward the request to.
    ///
    /// Typically a Datadog intake endpoint like:
    /// - `https://trace.agent.datadoghq.com/api/v0.2/traces`
    /// - `https://http-intake.logs.datadoghq.com/v1/input`
    pub target_url: String,

    /// Query parameters to include in the forwarded request.
    ///
    /// May include API keys, tags, or other metadata.
    pub query_params: HashMap<String, String>,
}

/// Aggregator for buffering HTTP proxy requests before forwarding.
///
/// This aggregator implements a FIFO queue for proxy requests with backpressure
/// via eviction when the queue is full.
///
/// # Example
///
/// ```rust
/// use datadog_agent_native::traces::proxy_aggregator::Aggregator;
///
/// let mut aggregator = Aggregator::default();
/// // aggregator.add(proxy_request);
/// let batch = aggregator.get_batch();
/// // Forward batch to Datadog...
/// ```
pub struct Aggregator {
    /// FIFO queue of proxy requests waiting to be forwarded.
    queue: VecDeque<ProxyRequest>,
    /// Maximum number of queued requests before eviction.
    max_queue_items: usize,
}

impl Default for Aggregator {
    /// Creates a new proxy aggregator with default settings.
    ///
    /// - Initial capacity: 128 requests (pre-allocated for efficiency)
    /// - Max queue items: 1,000 requests (`MAX_QUEUE_ITEMS`)
    fn default() -> Self {
        Aggregator {
            queue: VecDeque::with_capacity(128), // Pre-allocate for common case
            max_queue_items: MAX_QUEUE_ITEMS,
        }
    }
}

impl Aggregator {
    /// Adds an individual proxy request to the aggregation queue.
    ///
    /// If the queue is at capacity (`max_queue_items`), the oldest request is evicted
    /// (dropped) to prevent unbounded memory growth. This implements a FIFO eviction
    /// policy for backpressure under high load.
    ///
    /// # Arguments
    ///
    /// * `request` - Proxy request to buffer
    ///
    /// # Eviction Policy
    ///
    /// When the queue is full:
    /// 1. The oldest request (front of queue) is removed and dropped
    /// 2. A warning is logged
    /// 3. The new request is added to the back of the queue
    ///
    /// # Performance
    ///
    /// - Add: O(1) amortized (VecDeque push_back)
    /// - Evict: O(1) (VecDeque pop_front)
    pub fn add(&mut self, request: ProxyRequest) {
        // Check if queue is at capacity before adding new request
        if self.queue.len() >= self.max_queue_items {
            // Evict oldest request to make room (FIFO eviction policy)
            // O(1) operation with VecDeque
            self.queue.pop_front();
            warn!(
                "Proxy aggregator queue full ({} items), dropping oldest proxy request",
                self.max_queue_items
            );
        }
        // Add new request to the back of the queue
        self.queue.push_back(request);
    }

    /// Returns all buffered proxy requests as a batch.
    ///
    /// This drains the entire queue, returning all requests at once.
    /// The queue will be empty after this call.
    ///
    /// # Returns
    ///
    /// Vector of all queued proxy requests, in FIFO order (oldest first).
    ///
    /// # Performance
    ///
    /// - O(n) where n is the number of requests in the queue
    /// - Memory: Allocates a new Vec for the batch
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::proxy_aggregator::Aggregator;
    ///
    /// let mut aggregator = Aggregator::default();
    /// // Add requests...
    ///
    /// let batch = aggregator.get_batch();
    /// println!("Forwarding {} requests", batch.len());
    /// ```
    pub fn get_batch(&mut self) -> Vec<ProxyRequest> {
        // Drain entire queue into a Vec for compatibility with flusher
        self.queue.drain(..).collect()
    }

    /// Clears the aggregation queue, discarding all buffered requests.
    ///
    /// This is useful during shutdown or when resetting the aggregator state.
    /// All requests in the queue are dropped and will not be forwarded.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::proxy_aggregator::Aggregator;
    ///
    /// let mut aggregator = Aggregator::default();
    /// // Add requests...
    ///
    /// // Discard all buffered requests
    /// aggregator.clear();
    /// ```
    pub fn clear(&mut self) {
        self.queue.clear();
    }
}
