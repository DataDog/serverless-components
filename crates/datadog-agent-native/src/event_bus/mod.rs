//! Simple event bus for inter-component communication.
//!
//! This module provides a lightweight event bus implementation using Tokio's
//! MPSC channels for asynchronous event broadcasting. Components can send events
//! to notify other parts of the system about important occurrences.
//!
//! # Architecture
//!
//! The event bus uses a simple publish-subscribe pattern:
//! ```text
//! Producers (many)        EventBus         Consumer (one)
//!     │                      │                   │
//!     ├─ TraceReceived ─────>│                   │
//!     ├─ LogsFlushed ───────>│ ──> MPSC ──────> rx
//!     ├─ MetricsFlushed ────>│     channel       │
//!     └─ OutOfMemory ───────>│                   │
//! ```
//!
//! # Events
//!
//! The event bus supports various event types:
//! - **TraceReceived**: A trace was received (with trace ID and span count)
//! - **LogsFlushed**: A log batch was flushed (with count)
//! - **MetricsFlushed**: Metrics were flushed (with count)
//! - **OutOfMemory**: Out of memory condition detected (with PID)
//! - **Tombstone**: Shutdown signal
//!
//! # Usage
//!
//! ```rust,ignore
//! use datadog_agent_native::event_bus::{EventBus, Event};
//!
//! // Create the event bus and get a sender
//! let (event_bus, tx) = EventBus::run();
//!
//! // Send events from anywhere
//! tx.send(Event::TraceReceived { trace_id: 12345, span_count: 10 }).await?;
//!
//! // Receive events in the consumer
//! while let Some(event) = event_bus.rx.recv().await {
//!     match event {
//!         Event::TraceReceived { trace_id, span_count } => {
//!             println!("Received trace {} with {} spans", trace_id, span_count);
//!         }
//!         Event::Tombstone => break,
//!         _ => {}
//!     }
//! }
//! ```

use tokio::sync::mpsc::{self, Sender};

use crate::event_bus::constants::MAX_EVENTS;

mod constants;

/// Events that can be sent through the event bus.
///
/// These events represent significant occurrences in the agent's operation
/// that other components may need to be aware of.
///
/// # Event Types
///
/// - **TraceReceived**: Emitted when a new trace is received
/// - **LogsFlushed**: Emitted after logs are successfully flushed
/// - **MetricsFlushed**: Emitted after metrics are successfully flushed
/// - **OutOfMemory**: Emitted when an out-of-memory condition is detected
/// - **Tombstone**: Shutdown signal to terminate event processing
///
/// # Copy and Clone
///
/// Events are `Copy` and `Clone` because they contain only small, primitive data.
/// This allows events to be easily duplicated without heap allocation.
#[derive(Debug, Copy, Clone)]
pub enum Event {
    /// Trace received event with trace ID and span count.
    ///
    /// Emitted when the agent receives a new trace from the tracer.
    ///
    /// # Fields
    ///
    /// * `trace_id` - Unique identifier for the trace (64-bit)
    /// * `span_count` - Number of spans in this trace
    TraceReceived {
        /// Unique trace identifier
        trace_id: u64,
        /// Number of spans in the trace
        span_count: usize,
    },

    /// Log batch flushed event with count of logs flushed.
    ///
    /// Emitted after the logs agent successfully flushes a batch of logs
    /// to the Datadog backend.
    ///
    /// # Fields
    ///
    /// * `count` - Number of logs in the flushed batch
    LogsFlushed {
        /// Number of logs flushed
        count: usize,
    },

    /// Metrics flushed event with count of metrics flushed.
    ///
    /// Emitted after the metrics agent successfully flushes metrics
    /// to the Datadog backend.
    ///
    /// # Fields
    ///
    /// * `count` - Number of metrics in the flushed batch
    MetricsFlushed {
        /// Number of metrics flushed
        count: usize,
    },

    /// Out of memory warning with process ID.
    ///
    /// Emitted when the agent detects an out-of-memory condition,
    /// typically for a Lambda function process.
    ///
    /// # Fields
    ///
    /// * `0` - Process ID that ran out of memory
    OutOfMemory(i64),

    /// Shutdown signal for the event bus.
    ///
    /// This is a special event that signals the event bus consumer to
    /// terminate event processing. When received, the consumer should
    /// exit its event loop.
    Tombstone,
}

/// Event bus for inter-component communication.
///
/// The event bus provides a simple publish-subscribe mechanism where multiple
/// producers can send events to a single consumer via an MPSC channel.
///
/// # Architecture
///
/// - **Producer**: Any component with a `Sender<Event>` can send events
/// - **Consumer**: The component that owns the `EventBus` receives all events
/// - **Channel**: Backed by Tokio MPSC with bounded capacity (100 events)
///
/// # Example
///
/// ```rust,ignore
/// let (event_bus, tx) = EventBus::run();
///
/// // Producer sends events
/// tx.send(Event::TraceReceived { trace_id: 123, span_count: 5 }).await?;
///
/// // Consumer receives events
/// while let Some(event) = event_bus.rx.recv().await {
///     println!("Received event: {:?}", event);
/// }
/// ```
#[allow(clippy::module_name_repetitions)]
pub struct EventBus {
    /// Receiver for events sent through the event bus.
    ///
    /// The consumer should poll this receiver in a loop to process incoming events.
    pub rx: mpsc::Receiver<Event>,
}

impl EventBus {
    /// Creates a new event bus with an MPSC channel.
    ///
    /// This function creates a bounded MPSC channel with capacity [`MAX_EVENTS`]
    /// and returns both the event bus (receiver) and a sender for producers.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// * `EventBus` - The event bus receiver (owned by the consumer)
    /// * `Sender<Event>` - A sender that can be cloned and shared with producers
    ///
    /// # Channel Characteristics
    ///
    /// - **Capacity**: 100 events (bounded)
    /// - **Backpressure**: Senders block when channel is full
    /// - **Async**: Uses Tokio's async MPSC implementation
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (event_bus, tx) = EventBus::run();
    ///
    /// // Clone sender for multiple producers
    /// let tx1 = tx.clone();
    /// let tx2 = tx.clone();
    ///
    /// // Spawn producers
    /// tokio::spawn(async move {
    ///     tx1.send(Event::LogsFlushed { count: 10 }).await.ok();
    /// });
    ///
    /// // Consumer processes events
    /// while let Some(event) = event_bus.rx.recv().await {
    ///     println!("Event: {:?}", event);
    /// }
    /// ```
    #[must_use]
    pub fn run() -> (EventBus, Sender<Event>) {
        // Decision: Use bounded channel with MAX_EVENTS capacity (100)
        // This provides backpressure and prevents unbounded memory growth
        let (tx, rx) = mpsc::channel(MAX_EVENTS);

        // Decision: Wrap receiver in EventBus struct for clean API
        let event_bus = EventBus { rx };

        (event_bus, tx)
    }
}
