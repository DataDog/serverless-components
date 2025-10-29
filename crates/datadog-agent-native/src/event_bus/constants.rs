//! Constants for the event bus.
//!
//! This module contains configuration constants for the event bus,
//! including channel capacity and buffer sizes.

/// Maximum capacity of the event bus channel.
///
/// This defines the buffer size for the MPSC channel used by the event bus.
/// When the channel is full, senders will wait until space becomes available.
///
/// # Value
///
/// The default capacity is 100 events, which provides a reasonable balance between:
/// - Memory usage (each Event is small, ~24 bytes)
/// - Backpressure (prevents unbounded growth)
/// - Throughput (allows bursts of events)
///
/// # Performance Considerations
///
/// A capacity of 100 means the event bus can buffer up to 100 events before
/// senders block. In typical Lambda environments, this is more than sufficient
/// since events are processed quickly and invocations are serialized.
pub(crate) const MAX_EVENTS: usize = 100;
