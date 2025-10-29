//! Log aggregation and batching for efficient delivery to Datadog.
//!
//! This module provides buffering and batching of individual log entries into
//! JSON arrays that can be efficiently sent to Datadog's log intake API.
//!
//! # Batching Strategy
//!
//! Logs are aggregated until one of three limits is reached:
//! 1. **Count limit**: Maximum number of entries per batch (1,000)
//! 2. **Size limit**: Maximum uncompressed payload size (5MB)
//! 3. **Flush trigger**: Manual flush or periodic timer
//!
//! # Memory Management
//!
//! The aggregator implements FIFO eviction when the queue reaches 50,000 entries,
//! preventing unbounded memory growth under extreme log volume.
//!
//! # Output Format
//!
//! Batches are formatted as JSON arrays:
//! ```json
//! [
//!   {"message": "Log entry 1", "timestamp": 1234567890},
//!   {"message": "Log entry 2", "timestamp": 1234567891}
//! ]
//! ```

use std::collections::VecDeque;
use tracing::warn;

use crate::logs::constants;

/// Maximum number of log messages that can be queued before eviction.
///
/// When the queue reaches this size, oldest logs are evicted (dropped) using
/// a FIFO eviction policy to prevent unbounded memory growth.
///
/// # Value: 50,000 entries
///
/// Assuming average log size of ~1KB:
/// - `50,000 entries × 1KB = ~50MB` of buffered logs
///
/// This provides substantial buffering capacity while preventing excessive
/// memory usage under sustained high log volume.
///
/// # Eviction Behavior
///
/// When the queue is full and a new log arrives:
/// 1. The oldest log (front of queue) is removed and dropped
/// 2. A warning is logged indicating data loss
/// 3. The new log is added to the back of the queue
const MAX_LOG_QUEUE_SIZE: usize = 50_000;

/// Aggregates individual log entries into batches for efficient delivery.
///
/// The aggregator buffers log entries and constructs JSON array payloads
/// that respect Datadog's intake API size and count limits.
///
/// # Fields
///
/// - `messages`: FIFO queue of log entries (as JSON strings)
/// - `max_batch_entries_size`: Maximum entries per batch (typically 1,000)
/// - `max_content_size_bytes`: Maximum uncompressed payload size (typically 5MB)
/// - `max_log_size_bytes`: Maximum individual entry size (typically 1MB)
/// - `max_log_queue_size`: Maximum queued entries before eviction (50,000)
/// - `buffer`: Reusable buffer for constructing JSON array payloads
///
/// # Example
///
/// ```rust
/// use datadog_agent_native::logs::aggregator::Aggregator;
///
/// let mut aggregator = Aggregator::default();
///
/// // Add logs
/// aggregator.add_batch(vec![
///     r#"{"message":"Log 1"}"#.to_string(),
///     r#"{"message":"Log 2"}"#.to_string(),
/// ]);
///
/// // Get batch as JSON array
/// let batch = aggregator.get_batch();
/// // batch = b"[{\"message\":\"Log 1\"},{\"message\":\"Log 2\"}]"
/// ```
#[derive(Debug, Clone)]
pub struct Aggregator {
    /// FIFO queue of log entries waiting to be batched.
    ///
    /// Each entry is a JSON string representing a single log.
    pub(crate) messages: VecDeque<String>,

    /// Maximum number of log entries per batch.
    ///
    /// Typically set to [`constants::MAX_BATCH_ENTRIES_SIZE`] (1,000).
    pub(crate) max_batch_entries_size: usize,

    /// Maximum uncompressed payload size in bytes.
    ///
    /// Typically set to [`constants::MAX_CONTENT_SIZE_BYTES`] (5MB).
    pub(crate) max_content_size_bytes: usize,

    /// Maximum size of individual log entry in bytes.
    ///
    /// Entries exceeding this size trigger a warning.
    /// Typically set to [`constants::MAX_LOG_SIZE_BYTES`] (1MB).
    pub(crate) max_log_size_bytes: usize,

    /// Maximum number of queued log entries before FIFO eviction.
    ///
    /// Set to [`MAX_LOG_QUEUE_SIZE`] (50,000).
    pub(crate) max_log_queue_size: usize,

    /// Reusable buffer for constructing JSON array payloads.
    ///
    /// Pre-allocated to [`max_content_size_bytes`] for efficiency.
    pub(crate) buffer: Vec<u8>,
}

impl Default for Aggregator {
    /// Creates a new aggregator with default Datadog API limits.
    ///
    /// This is the recommended constructor for production use, as it
    /// uses the standard Datadog API constraints.
    ///
    /// # Default Values
    ///
    /// - **Max batch entries**: 1,000 ([`constants::MAX_BATCH_ENTRIES_SIZE`])
    /// - **Max content size**: 5MB ([`constants::MAX_CONTENT_SIZE_BYTES`])
    /// - **Max log size**: 1MB ([`constants::MAX_LOG_SIZE_BYTES`])
    /// - **Max queue size**: 50,000 ([`MAX_LOG_QUEUE_SIZE`])
    /// - **Buffer capacity**: 5MB (pre-allocated for efficiency)
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::logs::aggregator::Aggregator;
    ///
    /// let aggregator = Aggregator::default();
    /// ```
    fn default() -> Self {
        Aggregator {
            messages: VecDeque::new(),
            max_batch_entries_size: constants::MAX_BATCH_ENTRIES_SIZE,
            max_content_size_bytes: constants::MAX_CONTENT_SIZE_BYTES,
            max_log_size_bytes: constants::MAX_LOG_SIZE_BYTES,
            max_log_queue_size: MAX_LOG_QUEUE_SIZE,
            // Pre-allocate buffer to max content size for efficiency
            buffer: Vec::with_capacity(constants::MAX_CONTENT_SIZE_BYTES),
        }
    }
}

impl Aggregator {
    /// Creates a new aggregator with custom limits.
    ///
    /// Use this constructor only when you need non-standard limits
    /// (e.g., for testing or special deployment scenarios).
    /// For production use, prefer [`Aggregator::default()`].
    ///
    /// # Arguments
    ///
    /// * `max_batch_entries_size` - Maximum log entries per batch
    /// * `max_content_size_bytes` - Maximum uncompressed payload size
    /// * `max_log_size_bytes` - Maximum individual log entry size
    ///
    /// # Note
    ///
    /// The queue size is always set to [`MAX_LOG_QUEUE_SIZE`] (50,000)
    /// and cannot be customized.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::logs::aggregator::Aggregator;
    ///
    /// // Create aggregator with custom limits for testing
    /// let aggregator = Aggregator::new(
    ///     100,   // 100 entries per batch
    ///     1024,  // 1KB max payload
    ///     512,   // 512B max log entry
    /// );
    /// ```
    #[allow(dead_code)]
    #[allow(clippy::must_use_candidate)]
    pub fn new(
        max_batch_entries_size: usize,
        max_content_size_bytes: usize,
        max_log_size_bytes: usize,
    ) -> Self {
        Aggregator {
            messages: VecDeque::new(),
            max_batch_entries_size,
            max_content_size_bytes,
            max_log_size_bytes,
            max_log_queue_size: MAX_LOG_QUEUE_SIZE,
            // Pre-allocate buffer to max content size for efficiency
            buffer: Vec::with_capacity(max_content_size_bytes),
        }
    }

    /// Adds a batch of log entries to the aggregation queue.
    ///
    /// Logs are added to the queue in FIFO order. If the queue reaches
    /// capacity ([`MAX_LOG_QUEUE_SIZE`]), the oldest logs are evicted
    /// and dropped to prevent unbounded memory growth.
    ///
    /// # Arguments
    ///
    /// * `logs` - Vector of log entries as JSON strings
    ///
    /// # Eviction Policy
    ///
    /// When queue is full (50,000 entries):
    /// 1. Remove oldest log (front of queue) - O(1) with VecDeque
    /// 2. Log a warning indicating data loss
    /// 3. Add new log to back of queue
    ///
    /// This ensures the agent remains operational under extreme load
    /// at the cost of dropping the oldest logs.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::logs::aggregator::Aggregator;
    ///
    /// let mut aggregator = Aggregator::default();
    ///
    /// aggregator.add_batch(vec![
    ///     r#"{"message":"Error in service A"}"#.to_string(),
    ///     r#"{"message":"Request completed"}"#.to_string(),
    /// ]);
    /// ```
    pub fn add_batch(&mut self, logs: Vec<String>) {
        for log in logs {
            // Check if queue is at capacity
            if self.messages.len() >= self.max_log_queue_size {
                // Evict oldest log (FIFO eviction policy) - O(1) with VecDeque
                self.messages.pop_front();
                warn!(
                    "Log aggregator queue full ({} items), dropping oldest log message",
                    self.max_log_queue_size
                );
            }
            self.messages.push_back(log);
        }
    }

    /// Returns a batch of logs as a JSON array, respecting size and count limits.
    ///
    /// This method constructs a JSON array from queued logs, draining them from
    /// the queue until either the count limit or size limit is reached.
    ///
    /// # Returns
    ///
    /// - **Vec<u8>**: JSON array of logs as bytes, e.g., `[{...},{...}]`
    /// - **Empty Vec**: If queue is empty or first log exceeds size limit
    ///
    /// # Batching Rules
    ///
    /// The batch is built until one of these conditions is met:
    /// 1. **Count limit**: Reached `max_batch_entries_size` entries
    /// 2. **Size limit**: Next log would exceed `max_content_size_bytes`
    /// 3. **Queue empty**: No more logs available
    ///
    /// # Output Format
    ///
    /// ```json
    /// [{"message":"log1"},{"message":"log2"}]
    /// ```
    ///
    /// Empty queue returns empty vector (not `[]`).
    ///
    /// # Buffer Reuse
    ///
    /// The internal buffer is cleared and reused across calls for efficiency,
    /// avoiding allocations on every batch.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::logs::aggregator::Aggregator;
    ///
    /// let mut aggregator = Aggregator::default();
    /// aggregator.add_batch(vec![
    ///     r#"{"message":"log1"}"#.to_string(),
    ///     r#"{"message":"log2"}"#.to_string(),
    /// ]);
    ///
    /// let batch = aggregator.get_batch();
    /// // batch = b"[{\"message\":\"log1\"},{\"message\":\"log2\"}]"
    /// ```
    pub fn get_batch(&mut self) -> Vec<u8> {
        // Start JSON array
        self.buffer.extend(b"[");

        // Fill the batch with logs from the queue
        // Loop up to max_batch_entries_size times (count limit)
        for _ in 0..self.max_batch_entries_size {
            // Try to get next log from queue (FIFO order)
            if let Some(log) = self.messages.pop_front() {
                // Check if adding this log would exceed size limit
                // Decision: Check before adding to ensure we stay under limit
                if self.buffer.len() + log.len() > self.max_content_size_bytes {
                    // Log won't fit - put it back at front of queue
                    // This log will be included in the next batch
                    self.messages.push_front(log);
                    break; // Stop building this batch
                }

                // Warn if log exceeds recommended size
                // Note: We still include it in the batch (backend will truncate)
                if log.len() > self.max_log_size_bytes {
                    warn!(
                        "Log size exceeds the 1MB limit: {}, will be truncated by the backend.",
                        log.len()
                    );
                }

                // Add log to buffer and append comma separator
                self.buffer.extend(log.as_bytes());
                self.buffer.extend(b",");
            } else {
                // Queue is empty - stop building batch
                break;
            }
        }

        // Finalize JSON array
        // Check if we added at least one log (buffer length > 1 means "[" + content)
        if self.buffer.len() > 1 {
            // Remove trailing comma from last log entry
            self.buffer.pop();
            // Close JSON array bracket
            self.buffer.extend(b"]");
        } else {
            // No logs were added (queue was empty)
            // Remove opening bracket to return empty vector
            self.buffer.pop();
        }

        // Take ownership of buffer contents and leave empty buffer behind
        // This reuses the buffer allocation for next batch (efficiency)
        std::mem::take(&mut self.buffer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn create_test_aggregator() -> Aggregator {
        Aggregator::new(10, 100, 50)
    }

    fn create_log_entry(content: &str) -> String {
        format!("{{\"message\":\"{}\"}}", content)
    }

    #[test]
    fn test_aggregator_default() {
        let aggregator = Aggregator::default();

        assert_eq!(
            aggregator.max_batch_entries_size,
            constants::MAX_BATCH_ENTRIES_SIZE
        );
        assert_eq!(
            aggregator.max_content_size_bytes,
            constants::MAX_CONTENT_SIZE_BYTES
        );
        assert_eq!(aggregator.max_log_size_bytes, constants::MAX_LOG_SIZE_BYTES);
        assert!(aggregator.messages.is_empty());
        assert_eq!(
            aggregator.buffer.capacity(),
            constants::MAX_CONTENT_SIZE_BYTES
        );
    }

    #[test]
    fn test_aggregator_new() {
        let max_batch = 100;
        let max_content = 1024;
        let max_log = 512;

        let aggregator = Aggregator::new(max_batch, max_content, max_log);

        assert_eq!(aggregator.max_batch_entries_size, max_batch);
        assert_eq!(aggregator.max_content_size_bytes, max_content);
        assert_eq!(aggregator.max_log_size_bytes, max_log);
        assert!(aggregator.messages.is_empty());
        assert_eq!(aggregator.buffer.capacity(), max_content);
    }

    #[test]
    fn test_add_batch_empty() {
        let mut aggregator = create_test_aggregator();

        aggregator.add_batch(vec![]);

        assert!(aggregator.messages.is_empty());
    }

    #[test]
    fn test_add_batch_single_log() {
        let mut aggregator = create_test_aggregator();
        let log = create_log_entry("test message");

        aggregator.add_batch(vec![log.clone()]);

        assert_eq!(aggregator.messages.len(), 1);
        assert_eq!(aggregator.messages[0], log);
    }

    #[test]
    fn test_add_batch_multiple_logs() {
        let mut aggregator = create_test_aggregator();
        let logs = vec![
            create_log_entry("message 1"),
            create_log_entry("message 2"),
            create_log_entry("message 3"),
        ];

        aggregator.add_batch(logs.clone());

        assert_eq!(aggregator.messages.len(), 3);
        for (i, log) in logs.iter().enumerate() {
            assert_eq!(&aggregator.messages[i], log);
        }
    }

    #[test]
    fn test_add_batch_multiple_calls() {
        let mut aggregator = create_test_aggregator();

        aggregator.add_batch(vec![create_log_entry("batch 1 - log 1")]);
        aggregator.add_batch(vec![
            create_log_entry("batch 2 - log 1"),
            create_log_entry("batch 2 - log 2"),
        ]);

        assert_eq!(aggregator.messages.len(), 3);
    }

    #[test]
    fn test_get_batch_empty() {
        let mut aggregator = create_test_aggregator();

        let batch = aggregator.get_batch();

        assert!(batch.is_empty());
        assert!(aggregator.buffer.is_empty());
    }

    #[test]
    fn test_get_batch_single_log() {
        let mut aggregator = create_test_aggregator();
        let log = create_log_entry("test");
        aggregator.add_batch(vec![log.clone()]);

        let batch = aggregator.get_batch();

        let expected = format!("[{}]", log);
        assert_eq!(String::from_utf8(batch).unwrap(), expected);
        assert!(aggregator.messages.is_empty());
    }

    #[test]
    fn test_get_batch_multiple_logs() {
        let mut aggregator = create_test_aggregator();
        let logs = vec![
            create_log_entry("msg1"),
            create_log_entry("msg2"),
            create_log_entry("msg3"),
        ];
        aggregator.add_batch(logs.clone());

        let batch = aggregator.get_batch();

        let expected = format!("[{},{},{}]", logs[0], logs[1], logs[2]);
        assert_eq!(String::from_utf8(batch).unwrap(), expected);
        assert!(aggregator.messages.is_empty());
    }

    #[test]
    fn test_get_batch_respects_max_entries() {
        let mut aggregator = Aggregator::new(2, 1000, 500);
        let logs = vec![
            create_log_entry("msg1"),
            create_log_entry("msg2"),
            create_log_entry("msg3"),
            create_log_entry("msg4"),
        ];
        aggregator.add_batch(logs.clone());

        let batch = aggregator.get_batch();

        // Should only contain first 2 logs
        let expected = format!("[{},{}]", logs[0], logs[1]);
        assert_eq!(String::from_utf8(batch).unwrap(), expected);
        // Remaining 2 logs should still be in queue
        assert_eq!(aggregator.messages.len(), 2);
    }

    #[test]
    fn test_get_batch_respects_max_content_size() {
        // Set max content size to 35 bytes
        // Each log is ~15 bytes: {"message":"a"}
        // "[" (1) + log1 (15) + "," (1) + log2 (15) + "]" (1) = 33 bytes - fits
        // "[" (1) + log1 (15) + "," (1) + log2 (15) + "," (1) + log3 (15) + "]" (1) = 50 bytes - doesn't fit
        let mut aggregator = Aggregator::new(100, 35, 100);
        let logs = vec![
            create_log_entry("a"), // 15 bytes
            create_log_entry("b"), // 15 bytes
            create_log_entry("c"), // 15 bytes
        ];
        aggregator.add_batch(logs.clone());

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Should be able to fit first two logs (33 bytes total)
        assert!(result.starts_with('['));
        assert!(result.ends_with(']'));
        assert!(result.len() <= 35);
        // One log should remain in queue
        assert_eq!(aggregator.messages.len(), 1);
    }

    #[test]
    fn test_get_batch_log_too_large_puts_back() {
        // Create aggregator with small content size
        let mut aggregator = Aggregator::new(100, 30, 100);
        let large_log =
            create_log_entry("this is a very long message that exceeds the max content size");
        aggregator.add_batch(vec![large_log.clone()]);

        let batch = aggregator.get_batch();

        // Batch should be empty since log couldn't fit
        assert!(batch.is_empty());
        // Log should be put back in queue
        assert_eq!(aggregator.messages.len(), 1);
        assert_eq!(aggregator.messages[0], large_log);
    }

    #[test]
    fn test_get_batch_multiple_calls() {
        let mut aggregator = Aggregator::new(2, 1000, 500);
        let logs = vec![
            create_log_entry("msg1"),
            create_log_entry("msg2"),
            create_log_entry("msg3"),
            create_log_entry("msg4"),
        ];
        aggregator.add_batch(logs.clone());

        // First batch
        let batch1 = aggregator.get_batch();
        let result1 = String::from_utf8(batch1).unwrap();
        assert_eq!(result1, format!("[{},{}]", logs[0], logs[1]));

        // Second batch
        let batch2 = aggregator.get_batch();
        let result2 = String::from_utf8(batch2).unwrap();
        assert_eq!(result2, format!("[{},{}]", logs[2], logs[3]));

        // Third batch should be empty
        let batch3 = aggregator.get_batch();
        assert!(batch3.is_empty());
    }

    #[test]
    fn test_get_batch_buffer_reuse() {
        let mut aggregator = create_test_aggregator();
        let log = create_log_entry("test");

        // First batch
        aggregator.add_batch(vec![log.clone()]);
        let batch1 = aggregator.get_batch();
        assert!(!batch1.is_empty());

        // Second batch - buffer should be reused (cleared)
        aggregator.add_batch(vec![log.clone()]);
        let batch2 = aggregator.get_batch();
        let result = String::from_utf8(batch2).unwrap();

        // Should only contain one log, not two
        assert_eq!(result, format!("[{}]", log));
    }

    #[test]
    fn test_get_batch_json_format() {
        let mut aggregator = create_test_aggregator();
        aggregator.add_batch(vec![
            "{\"message\":\"log1\",\"level\":\"info\"}".to_string(),
            "{\"message\":\"log2\",\"level\":\"error\"}".to_string(),
        ]);

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Verify it's valid JSON array format
        assert!(result.starts_with('['));
        assert!(result.ends_with(']'));
        assert!(result.contains(','));

        // Verify it can be parsed as JSON
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_get_batch_preserves_order() {
        let mut aggregator = create_test_aggregator();
        let logs = vec![
            create_log_entry("first"),
            create_log_entry("second"),
            create_log_entry("third"),
        ];
        aggregator.add_batch(logs.clone());

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Verify logs are in FIFO order
        assert_eq!(result, format!("[{},{},{}]", logs[0], logs[1], logs[2]));
    }

    #[test]
    fn test_aggregator_clone() {
        let mut aggregator = create_test_aggregator();
        aggregator.add_batch(vec![create_log_entry("test")]);

        let cloned = aggregator.clone();

        assert_eq!(cloned.messages.len(), aggregator.messages.len());
        assert_eq!(
            cloned.max_batch_entries_size,
            aggregator.max_batch_entries_size
        );
        assert_eq!(
            cloned.max_content_size_bytes,
            aggregator.max_content_size_bytes
        );
        assert_eq!(cloned.max_log_size_bytes, aggregator.max_log_size_bytes);
    }

    #[test]
    fn test_get_batch_with_varying_sizes() {
        let mut aggregator = Aggregator::new(100, 200, 100);
        let logs = vec![
            create_log_entry("short"),
            create_log_entry("this is a medium length message"),
            create_log_entry("a"),
            create_log_entry("this is another medium length message for testing"),
        ];
        aggregator.add_batch(logs);

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Should fit as many as possible within size limit
        assert!(result.starts_with('['));
        assert!(result.ends_with(']'));
        assert!(result.len() <= 200);
    }

    #[test]
    fn test_get_batch_exactly_at_max_entries() {
        let mut aggregator = Aggregator::new(3, 1000, 500);
        let logs = vec![
            create_log_entry("1"),
            create_log_entry("2"),
            create_log_entry("3"),
        ];
        aggregator.add_batch(logs.clone());

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        assert_eq!(result, format!("[{},{},{}]", logs[0], logs[1], logs[2]));
        assert!(aggregator.messages.is_empty());
    }

    #[test]
    fn test_get_batch_with_special_characters() {
        let mut aggregator = create_test_aggregator();
        let logs = vec![
            r#"{"message":"Line with \"quotes\""}"#.to_string(),
            r#"{"message":"Line with newline"}"#.to_string(),
            r#"{"message":"Line with tab"}"#.to_string(),
        ];
        aggregator.add_batch(logs.clone());

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Should handle special characters properly - they are part of the JSON strings
        assert!(result.contains(r#"\"quotes\""#));
        assert!(result.contains("newline"));
        assert!(result.contains("tab"));

        // Verify proper JSON array format
        assert!(result.starts_with('['));
        assert!(result.ends_with(']'));
    }

    #[test]
    fn test_aggregator_debug_impl() {
        let aggregator = create_test_aggregator();
        let debug_str = format!("{:?}", aggregator);

        // Should contain struct name
        assert!(debug_str.contains("Aggregator"));
    }

    #[test]
    fn test_get_batch_interleaved_add_and_get() {
        let mut aggregator = Aggregator::new(2, 1000, 500);

        // Add first batch
        aggregator.add_batch(vec![create_log_entry("1"), create_log_entry("2")]);
        let batch1 = aggregator.get_batch();
        assert!(!batch1.is_empty());

        // Add more and get again
        aggregator.add_batch(vec![create_log_entry("3")]);
        let batch2 = aggregator.get_batch();
        let result2 = String::from_utf8(batch2).unwrap();
        assert_eq!(result2, format!("[{}]", create_log_entry("3")));

        // Add multiple and get with limit
        aggregator.add_batch(vec![
            create_log_entry("4"),
            create_log_entry("5"),
            create_log_entry("6"),
        ]);
        let batch3 = aggregator.get_batch();
        let result3 = String::from_utf8(batch3).unwrap();
        assert_eq!(
            result3,
            format!("[{},{}]", create_log_entry("4"), create_log_entry("5"))
        );

        // One more should remain
        assert_eq!(aggregator.messages.len(), 1);
    }

    #[test]
    fn test_get_batch_with_empty_strings() {
        let mut aggregator = create_test_aggregator();
        aggregator.add_batch(vec!["{}".to_string(), "{}".to_string()]);

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        assert_eq!(result, "[{},{}]");
    }

    #[test]
    fn test_max_log_size_warning() {
        // This test verifies that logs exceeding max_log_size_bytes are still processed
        // but would trigger a warning (we can't easily test the warning itself)
        let mut aggregator = Aggregator::new(100, 1000, 10);
        let large_log = create_log_entry("this is definitely longer than 10 bytes");
        aggregator.add_batch(vec![large_log.clone()]);

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Log should still be included despite exceeding max_log_size_bytes
        assert_eq!(result, format!("[{}]", large_log));
    }

    #[test]
    fn test_get_batch_boundary_conditions() {
        // Test when log size exactly equals remaining buffer space
        let log = create_log_entry("x"); // ~17 bytes
        let log_size = log.len();

        // Set max_content_size to exactly fit opening bracket + one log + closing bracket
        // "[" (1) + log (17) + "]" (1) = 19
        let mut aggregator = Aggregator::new(100, log_size + 2, 100);
        aggregator.add_batch(vec![log.clone(), create_log_entry("y")]);

        let batch = aggregator.get_batch();
        let result = String::from_utf8(batch).unwrap();

        // Should fit exactly one log
        assert_eq!(result, format!("[{}]", log));
        assert_eq!(aggregator.messages.len(), 1);
    }
}
