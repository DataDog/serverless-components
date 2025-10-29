//! Constants for Datadog Logs API limits.
//!
//! This module defines size and count limits for log payloads and batches,
//! based on Datadog's intake API constraints.
//!
//! # API Constraints
//!
//! The Datadog Logs API enforces several limits to ensure reliable ingestion:
//! - **Payload size**: Maximum uncompressed size per POST request
//! - **Entry size**: Maximum size of a single log entry
//! - **Batch count**: Maximum number of log entries per batch
//!
//! These constants ensure the agent respects these limits and prevents
//! rejected payloads.
//!
//! # Design Considerations
//!
//! - **5MB payload limit**: Balances network efficiency with memory usage
//! - **1MB entry limit**: Prevents individual logs from dominating batches
//! - **1,000 entry limit**: Ensures reasonable batch sizes for processing

/// Maximum content size per payload (uncompressed) in bytes.
///
/// This is the maximum size that the Datadog Logs API accepts for a single
/// POST request to the intake endpoint. Payloads exceeding this size will
/// be rejected with a 413 (Payload Too Large) error.
///
/// # Value: 5MB (5,242,880 bytes)
///
/// This limit allows for efficient batching while staying within API constraints.
/// The agent will split large batches to stay under this limit.
///
/// # Related
///
/// - See [`MAX_BATCH_ENTRIES_SIZE`] for count-based limits
/// - See [`MAX_LOG_SIZE_BYTES`] for individual entry limits
pub(crate) const MAX_CONTENT_SIZE_BYTES: usize = 5 * 1_024 * 1_024;

/// Maximum size in bytes of a single log entry before truncation.
///
/// Individual log entries exceeding this size will be truncated to prevent
/// them from dominating batches or causing API rejections.
///
/// # Value: 1MB (1,048,576 bytes)
///
/// This ensures that:
/// - A single log cannot consume the entire payload budget
/// - Memory usage per entry is bounded
/// - Large stack traces or error messages are still captured (1MB is generous)
///
/// # Truncation Behavior
///
/// When a log exceeds this size:
/// 1. The message is truncated to `MAX_LOG_SIZE_BYTES`
/// 2. A warning may be logged (implementation-dependent)
/// 3. The truncated log is sent to Datadog
///
/// # Related
///
/// - See [`MAX_CONTENT_SIZE_BYTES`] for total payload limits
pub(crate) const MAX_LOG_SIZE_BYTES: usize = 1_024 * 1_024;

/// Maximum number of log entries per batch.
///
/// This is the maximum number of individual log entries that can be included
/// in a single batch sent to the Datadog Logs API.
///
/// # Value: 1,000 entries
///
/// This limit ensures:
/// - Reasonable processing time per batch
/// - Manageable memory usage
/// - Efficient batching without excessive overhead
///
/// # Batching Strategy
///
/// The agent batches logs until either:
/// - The count reaches `MAX_BATCH_ENTRIES_SIZE`, OR
/// - The total size reaches `MAX_CONTENT_SIZE_BYTES`, OR
/// - A flush is triggered (timer or manual)
///
/// Whichever limit is hit first determines the batch boundary.
///
/// # Related
///
/// - See [`MAX_CONTENT_SIZE_BYTES`] for size-based limits
pub(crate) const MAX_BATCH_ENTRIES_SIZE: usize = 1000;
