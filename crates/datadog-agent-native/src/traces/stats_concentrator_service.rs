//! Stats concentrator service for aggregating span statistics into time buckets.
//!
//! This module provides a background service that aggregates span-level statistics
//! from both client-computed and agent-generated stats into time-bucketed metrics.
//! The concentrator is the central aggregation point for all APM statistics.
//!
//! # Architecture
//!
//! The concentrator service implements an actor pattern using async channels:
//! - **Handle**: Clients send commands (add span, flush) via async channel
//! - **Service**: Background task processes commands sequentially
//! - **Concentrator**: Underlying stats aggregator (from `datadog_trace_stats`)
//!
//! ```text
//! Stats Sources → Handle → Command Queue → Service → SpanConcentrator → Buckets
//!                    ↓                                                      ↓
//!              (try_send)                                            (10s buckets)
//! ```
//!
//! # Time Bucketing
//!
//! Statistics are aggregated into 10-second time buckets:
//! - Each bucket contains aggregated metrics for that time period
//! - Buckets are flushed periodically or on demand
//! - Metrics include: hit count, error count, duration distribution
//!
//! # Lock-Free Design
//!
//! The service uses message passing instead of mutexes:
//! - Avoids lock contention under high load
//! - Provides backpressure via bounded channel
//! - Sequential processing ensures consistency
//!
//! # Backpressure
//!
//! When the command channel is full (1,000 commands):
//! - `try_send` fails immediately (no blocking)
//! - Spans/metadata are dropped with warning logged
//! - Flush commands use blocking `send` (critical path)

use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use datadog_trace_protobuf::pb;
use datadog_trace_protobuf::pb::{ClientStatsPayload, TracerPayload};
use datadog_trace_stats::span_concentrator::SpanConcentrator;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{error, warn};

/// Nanoseconds per second conversion factor.
const S_TO_NS: u64 = 1_000_000_000;

/// Duration of each stats bucket in nanoseconds (10 seconds).
///
/// Statistics are aggregated into 10-second buckets. This matches the
/// Datadog backend's expectations for stats granularity.
const BUCKET_DURATION_NS: u64 = 10 * S_TO_NS; // 10 seconds

/// Buffer size for the stats concentrator command channel.
///
/// This prevents unbounded memory growth under high load by implementing
/// backpressure. When the channel is full, `try_send` will fail and
/// spans/metadata will be dropped.
///
/// With 1,000 commands and ~1KB per command: ~1MB channel buffer.
const STATS_CHANNEL_BUFFER_SIZE: usize = 1000;

/// Errors that can occur during stats concentrator operations.
#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    /// Failed to send command to the concentrator service.
    ///
    /// This typically means the service task has stopped or the channel
    /// buffer is full.
    #[error("Failed to send command to concentrator: {0}")]
    SendError(mpsc::error::SendError<ConcentratorCommand>),

    /// Failed to receive response from the concentrator service.
    ///
    /// This typically means the service task panicked or was cancelled
    /// before sending a response.
    #[error("Failed to receive response from concentrator: {0}")]
    RecvError(oneshot::error::RecvError),
}

/// Tracer metadata for identifying the source of statistics.
///
/// This metadata is set once per trace batch and applies to all spans
/// from that tracer instance.
#[derive(Clone, Debug, Default)]
pub struct TracerMetadata {
    /// Programming language of the tracer.
    ///
    /// Examples: `"python"`, `"java"`, `"ruby"`, `"go"`
    pub language: String,

    /// Version of the tracer library.
    ///
    /// Example: `"1.15.0"`
    pub tracer_version: String,

    /// Unique identifier for this tracer runtime instance.
    ///
    /// Generated once per application instance, used to track tracer
    /// lifecycles and distinguish multiple instances of the same service.
    ///
    /// Example: `"f45568ad09d5480b99087d86ebda26e6"`
    pub runtime_id: String,

    /// Container ID if running in a containerized environment.
    ///
    /// Used for container-level aggregation and tagging.
    pub container_id: String,
}

/// Commands sent to the stats concentrator service.
///
/// These commands are processed sequentially by the service task to avoid
/// the need for mutexes and prevent lock contention.
pub enum ConcentratorCommand {
    /// Set or update tracer metadata.
    ///
    /// This is typically sent once per trace batch before adding spans.
    SetTracerMetadata(TracerMetadata),

    /// Add a span for stats aggregation.
    ///
    /// The span is boxed to reduce the size of the enum (spans can be large).
    Add(Box<pb::Span>),

    /// Flush aggregated statistics.
    ///
    /// # Arguments
    /// - `bool`: Force flush (flush all buckets) or normal flush (only completed buckets)
    /// - `oneshot::Sender`: Channel to send back the flushed stats payload
    Flush(bool, oneshot::Sender<Option<ClientStatsPayload>>),
}

/// Handle for sending commands to the stats concentrator service.
///
/// This handle can be cloned and shared across multiple threads/tasks to send
/// spans and flush commands to the concentrator service. It implements a lock-free
/// design using message passing instead of mutexes.
///
/// # Cloning
///
/// The handle can be cloned cheaply (only clones the channel sender). Each clone
/// shares access to the same underlying service but maintains its own
/// `is_tracer_metadata_set` flag to minimize redundant metadata updates.
///
/// # Backpressure
///
/// - `add()` and `set_tracer_metadata()` use non-blocking `try_send`
/// - If channel is full, spans/metadata are dropped with warning
/// - `flush()` uses blocking `send` to ensure flushes complete
pub struct StatsConcentratorHandle {
    /// Channel sender for commands to the concentrator service.
    tx: mpsc::Sender<ConcentratorCommand>,
    /// Atomic flag to track if tracer metadata has been set.
    ///
    /// Used to avoid sending redundant metadata updates. Set once per handle
    /// when the first trace is received.
    is_tracer_metadata_set: AtomicBool,
}

impl Clone for StatsConcentratorHandle {
    /// Creates a clone of the handle with a cloned channel sender.
    ///
    /// The cloned handle shares access to the same concentrator service but maintains
    /// its own `is_tracer_metadata_set` flag. This may cause metadata to be sent
    /// multiple times (once per clone), but this is acceptable since:
    /// - Metadata is identical across all clones for the same tracer
    /// - The overhead of redundant metadata is minimal
    /// - Perfect deduplication across clones isn't required
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            // Cloning this may cause trace metadata to be set multiple times,
            // but it's okay because it's the same for all traces and we don't need to be perfect on dedup.
            is_tracer_metadata_set: AtomicBool::new(
                self.is_tracer_metadata_set.load(Ordering::Acquire),
            ),
        }
    }
}

impl StatsConcentratorHandle {
    /// Creates a new handle for the stats concentrator service.
    ///
    /// # Arguments
    ///
    /// * `tx` - Channel sender for sending commands to the concentrator service
    ///
    /// # Returns
    ///
    /// A new handle with the tracer metadata flag initialized to `false`.
    #[must_use]
    pub fn new(tx: mpsc::Sender<ConcentratorCommand>) -> Self {
        Self {
            tx,
            is_tracer_metadata_set: AtomicBool::new(false),
        }
    }

    /// Sets or updates tracer metadata in the concentrator.
    ///
    /// This method extracts metadata (language, tracer version, runtime ID, container ID)
    /// from the trace payload and sends it to the concentrator. To avoid redundant updates,
    /// metadata is only sent once per handle instance (tracked via atomic flag).
    ///
    /// # Arguments
    ///
    /// * `trace` - Tracer payload containing metadata to extract
    ///
    /// # Returns
    ///
    /// - `Ok(())` - Always returns success (errors are logged but not propagated)
    ///
    /// # Backpressure
    ///
    /// Uses non-blocking `try_send`. If the channel is full, the metadata update is
    /// dropped with a warning logged. This is acceptable because:
    /// - Metadata rarely changes
    /// - The service will use default/previous metadata
    /// - Dropping metadata is better than blocking trace processing
    ///
    /// # Thread Safety
    ///
    /// Safe to call from multiple threads. Uses atomic operations for the flag check.
    pub fn set_tracer_metadata(&self, trace: &TracerPayload) -> Result<(), StatsError> {
        // Set tracer metadata only once for the first trace because
        // it is the same for all traces.
        if !self.is_tracer_metadata_set.load(Ordering::Acquire) {
            self.is_tracer_metadata_set.store(true, Ordering::Release);
            let tracer_metadata = TracerMetadata {
                language: trace.language_name.clone(),
                tracer_version: trace.tracer_version.clone(),
                runtime_id: trace.runtime_id.clone(),
                container_id: trace.container_id.clone(),
            };
            // Use try_send to avoid blocking - if channel is full, log and continue
            if let Err(e) = self
                .tx
                .try_send(ConcentratorCommand::SetTracerMetadata(tracer_metadata))
            {
                warn!(
                    "Stats concentrator channel full, dropping tracer metadata: {}",
                    e
                );
            }
        }
        Ok(())
    }

    /// Adds a span for stats aggregation.
    ///
    /// The span is sent to the concentrator service where statistics will be extracted
    /// (duration, error status, service/operation tags) and aggregated into time buckets.
    ///
    /// # Arguments
    ///
    /// * `span` - Span to extract stats from
    ///
    /// # Returns
    ///
    /// - `Ok(())` - Always returns success (errors are logged but not propagated)
    ///
    /// # Backpressure
    ///
    /// Uses non-blocking `try_send`. If the channel is full, the span is dropped with
    /// a warning logged. This implements backpressure by dropping the oldest data when
    /// under load. Dropping spans for stats is preferable to:
    /// - Blocking trace processing
    /// - Causing memory pressure
    /// - Slowing down the hot path
    ///
    /// # Performance
    ///
    /// - Clones the span (required for sending across channel)
    /// - Boxes the span to reduce enum size
    /// - Non-blocking: O(1) operation
    pub fn add(&self, span: &pb::Span) -> Result<(), StatsError> {
        // Use try_send to avoid blocking - if channel is full, drop oldest spans
        if let Err(e) = self
            .tx
            .try_send(ConcentratorCommand::Add(Box::new(span.clone())))
        {
            warn!(
                "Stats concentrator channel full, dropping span for stats: {}",
                e
            );
        }
        Ok(())
    }

    /// Flushes aggregated statistics from the concentrator.
    ///
    /// This triggers the concentrator to return all completed time buckets as a
    /// `ClientStatsPayload` ready to be sent to Datadog.
    ///
    /// # Arguments
    ///
    /// * `force_flush` - If `true`, flush all buckets including incomplete ones.
    ///                   If `false`, only flush completed buckets.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(payload))` - Statistics payload with aggregated buckets
    /// - `Ok(None)` - No statistics available (empty buckets)
    /// - `Err(SendError)` - Failed to send flush command (service stopped)
    /// - `Err(RecvError)` - Failed to receive response (service panicked/cancelled)
    ///
    /// # Blocking Behavior
    ///
    /// Unlike `add()` and `set_tracer_metadata()`, this method uses blocking async `send`
    /// to ensure flush commands are never dropped. Flush is on the critical path for
    /// data delivery and must complete reliably.
    ///
    /// # Force Flush
    ///
    /// - Normal flush (`false`): Only completed 10-second buckets are returned
    /// - Force flush (`true`): All buckets including the current incomplete bucket
    ///
    /// Force flush is typically used during shutdown to avoid losing recent stats.
    pub async fn flush(&self, force_flush: bool) -> Result<Option<ClientStatsPayload>, StatsError> {
        let (response_tx, response_rx) = oneshot::channel();
        // Flush commands are critical, so use blocking send (async)
        self.tx
            .send(ConcentratorCommand::Flush(force_flush, response_tx))
            .await
            .map_err(StatsError::SendError)?;
        response_rx.await.map_err(StatsError::RecvError)
    }
}

/// Stats concentrator service implementing the actor pattern.
///
/// This service runs as a background task, processing commands from handles to
/// aggregate span statistics into time-bucketed metrics. It owns the underlying
/// `SpanConcentrator` and ensures sequential, lock-free access.
///
/// # Actor Pattern
///
/// The service implements the actor pattern:
/// - **Single Owner**: Only the service task accesses the concentrator directly
/// - **Message Queue**: Commands arrive via async channel (mpsc)
/// - **Sequential Processing**: Commands processed one at a time (no locks needed)
/// - **Isolation**: No shared mutable state between tasks
///
/// # Lifecycle
///
/// 1. Create service and handle via `new()`
/// 2. Spawn service with `tokio::spawn(service.run())`
/// 3. Use handles to send commands (add span, flush)
/// 4. Service runs until all handles are dropped (channel closes)
pub struct StatsConcentratorService {
    /// Underlying stats aggregator that computes metrics and manages time buckets.
    concentrator: SpanConcentrator,
    /// Channel receiver for incoming commands from handles.
    rx: mpsc::Receiver<ConcentratorCommand>,
    /// Current tracer metadata (set by first trace, applies to all stats).
    tracer_metadata: TracerMetadata,
    /// Agent configuration (for service name, env, version in stats payload).
    config: Arc<Config>,
}

// A service that handles add() and flush() requests in the same queue,
// to avoid using mutex, which may cause lock contention.
impl StatsConcentratorService {
    /// Creates a new stats concentrator service and its handle.
    ///
    /// This initializes the service with the given configuration and creates a channel
    /// for command passing. The service owns the concentrator and must be spawned as
    /// a background task.
    ///
    /// # Arguments
    ///
    /// * `config` - Agent configuration for env, service, version metadata
    ///
    /// # Returns
    ///
    /// A tuple of `(service, handle)`:
    /// - `service`: The service instance to spawn with `tokio::spawn(service.run())`
    /// - `handle`: Cloneable handle for sending commands to the service
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use datadog_agent_native::traces::stats_concentrator_service::StatsConcentratorService;
    /// use datadog_agent_native::config::Config;
    ///
    /// # async fn example() {
    /// let config = Arc::new(Config::default());
    /// let (service, handle) = StatsConcentratorService::new(config);
    ///
    /// // Spawn the service in the background
    /// tokio::spawn(service.run());
    ///
    /// // Use handle to add spans and flush stats
    /// // handle.add(&span)?;
    /// // let stats = handle.flush(false).await?;
    /// # }
    /// ```
    #[must_use]
    pub fn new(config: Arc<Config>) -> (Self, StatsConcentratorHandle) {
        let (tx, rx) = mpsc::channel(STATS_CHANNEL_BUFFER_SIZE);
        let handle = StatsConcentratorHandle::new(tx);
        // TODO: set span_kinds_stats_computed and peer_tag_keys
        let concentrator = SpanConcentrator::new(
            Duration::from_nanos(BUCKET_DURATION_NS),
            SystemTime::now(),
            vec![],
            vec![],
        );
        let service: StatsConcentratorService = Self {
            concentrator,
            rx,
            // To be set when the first trace is received
            tracer_metadata: TracerMetadata::default(),
            config,
        };
        (service, handle)
    }

    /// Main service loop that processes commands from handles.
    ///
    /// This async function runs indefinitely, processing commands sequentially until
    /// all handles are dropped (channel closes). It should be spawned as a background
    /// task with `tokio::spawn(service.run())`.
    ///
    /// # Command Processing
    ///
    /// The service handles three types of commands:
    /// 1. **SetTracerMetadata**: Updates the tracer metadata for all subsequent stats
    /// 2. **Add**: Adds a span to the concentrator for stats extraction
    /// 3. **Flush**: Flushes aggregated stats buckets and sends response
    ///
    /// # Sequential Processing
    ///
    /// Commands are processed one at a time in the order received. This eliminates
    /// the need for mutexes and prevents lock contention, even under high load.
    ///
    /// # Graceful Shutdown
    ///
    /// The service exits when:
    /// - All handles are dropped (channel sender side closes)
    /// - `rx.recv()` returns `None`
    ///
    /// Any in-flight commands are processed before shutdown. To avoid losing recent
    /// stats, clients should send a final `Flush(true)` before dropping handles.
    pub async fn run(mut self) {
        // Process commands until channel closes (all handles dropped)
        while let Some(command) = self.rx.recv().await {
            match command {
                // Update tracer metadata for all subsequent stats
                ConcentratorCommand::SetTracerMetadata(tracer_metadata) => {
                    self.tracer_metadata = tracer_metadata;
                }
                // Add span to concentrator for stats extraction and aggregation
                ConcentratorCommand::Add(span) => self.concentrator.add_span(&*span),
                // Flush aggregated stats and send response back to caller
                ConcentratorCommand::Flush(force_flush, response_tx) => {
                    self.handle_flush(force_flush, response_tx);
                }
            }
        }
    }

    /// Handles a flush command by retrieving stats buckets and building the payload.
    ///
    /// This method:
    /// 1. Flushes the underlying concentrator to get completed time buckets
    /// 2. Builds a `ClientStatsPayload` with metadata from config and tracer
    /// 3. Sends the payload back through the oneshot channel
    ///
    /// # Arguments
    ///
    /// * `force_flush` - If `true`, flush all buckets. If `false`, only completed buckets.
    /// * `response_tx` - Oneshot channel to send the stats payload back to the caller
    ///
    /// # Payload Construction
    ///
    /// The `ClientStatsPayload` includes:
    /// - **Configuration**: env, version, service (from agent config)
    /// - **Tracer Metadata**: language, tracer version, runtime ID, container ID
    /// - **Stats Buckets**: Aggregated time buckets with span statistics
    /// - **Hostname**: Intentionally empty to allow backend aggregation
    ///
    /// # Empty Buckets
    ///
    /// If no stats are available (empty buckets), returns `None` instead of an
    /// empty payload. This avoids unnecessary network traffic and backend processing.
    ///
    /// # Error Handling
    ///
    /// If sending the response fails (receiver dropped), logs an error but continues.
    /// This can happen if the caller's await was cancelled or the task panicked.
    fn handle_flush(
        &mut self,
        force_flush: bool,
        response_tx: oneshot::Sender<Option<ClientStatsPayload>>,
    ) {
        // Flush the concentrator to get completed time buckets
        let stats_buckets = self.concentrator.flush(SystemTime::now(), force_flush);

        // Build the stats payload if we have data
        let stats = if stats_buckets.is_empty() {
            // No stats available - return None to avoid sending empty payload
            None
        } else {
            // Build ClientStatsPayload with metadata from config and tracer
            Some(ClientStatsPayload {
                // Do not set hostname so the trace stats backend can aggregate stats properly
                hostname: String::new(),
                env: self.config.env.clone().unwrap_or("unknown-env".to_string()),
                // Version is not in the trace payload. Need to read it from config.
                version: self.config.version.clone().unwrap_or_default(),
                lang: self.tracer_metadata.language.clone(),
                tracer_version: self.tracer_metadata.tracer_version.clone(),
                runtime_id: self.tracer_metadata.runtime_id.clone(),
                // Not supported yet
                sequence: 0,
                // Not supported yet
                agent_aggregation: String::new(),
                service: self.config.service.clone().unwrap_or_default(),
                container_id: self.tracer_metadata.container_id.clone(),
                // Not supported yet
                tags: vec![],
                // Not supported yet
                git_commit_sha: String::new(),
                // Not supported yet
                image_tag: String::new(),
                stats: stats_buckets,
                // Not supported yet
                process_tags: String::new(),
                // Not supported yet
                process_tags_hash: 0,
            })
        };

        // Send the response back to the caller
        let response = response_tx.send(stats);
        if let Err(e) = response {
            // Receiver was dropped (caller cancelled or panicked)
            error!("Failed to return trace stats: {e:?}");
        }
    }
}
