//! Actor-based service for tracking high-water mark usage metrics.
//!
//! This module implements an actor pattern for thread-safe metric updates without locks.
//! The service tracks the maximum (high-water mark) values for three key resources:
//! - `/tmp` directory usage
//! - File descriptor usage
//! - Thread count
//!
//! # Architecture
//!
//! ```text
//! Monitoring Loop(s)                       Actor                     Reporter
//!       │                                    │                          │
//!       ├─ poll /tmp (10ms intervals)       │                          │
//!       ├─ poll fd count                    │                          │
//!       ├─ poll thread count                │                          │
//!       │                                    │                          │
//!       ├──Command::Update────────────────>│                          │
//!       │                                    ├─ track max values        │
//!       │                                    │   (high-water marks)     │
//!       │                                    │                          │
//!       │                                    │    <──Command::Get───────┤
//!       │                                    ├─────UsageMetrics────────>│
//!       │                                    │                          │
//!       │                                    │    <──Command::Reset─────┤
//!       │                                    ├─ clear metrics           │
//!       │                                    │   (after flush)          │
//! ```
//!
//! # High-Water Mark Tracking
//!
//! The service only updates metrics when new values exceed previous maximums:
//! - Initial state: `{tmp_used: 0.0, fd_use: 0.0, threads_use: 0.0}`
//! - Update(tmp: 100, fd: 50, threads: 10) → `{100, 50, 10}`
//! - Update(tmp: 80, fd: 60, threads: 5) → `{100, 60, 10}` (only fd increased)
//!
//! This provides peak usage metrics for each flush period, critical for detecting
//! resource exhaustion and capacity planning.
//!
//! # Usage Pattern
//!
//! ```rust,ignore
//! use datadog_agent_native::metrics::enhanced::usage_metrics::EnhancedMetricsService;
//!
//! // Create service and handle
//! let (service, handle) = EnhancedMetricsService::new();
//!
//! // Spawn service actor in background
//! tokio::spawn(async move { service.run().await });
//!
//! // Monitoring loop
//! loop {
//!     tokio::time::sleep(Duration::from_millis(10)).await;
//!
//!     let tmp_used = get_tmp_used().ok();
//!     let fd_count = get_fd_count().ok();
//!     let thread_count = get_thread_count().ok();
//!
//!     handle.update_metrics(tmp_used, fd_count, thread_count)?;
//! }
//!
//! // Reporter thread
//! let metrics = handle.get_metrics().await?;
//! report_metrics(&metrics);
//! handle.reset_metrics()?; // Clear for next flush period
//! ```
//!
//! # Thread Safety
//!
//! The actor pattern ensures thread-safe updates without locks:
//! - Multiple monitoring loops can send updates concurrently
//! - The actor processes commands sequentially (FIFO order)
//! - No contention, no deadlocks, clear ownership

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error};

/// High-water mark usage metrics for resource monitoring.
///
/// This struct tracks the maximum observed values for three key resources:
/// - **tmp_used**: Peak `/tmp` directory usage (bytes)
/// - **fd_use**: Peak file descriptor count
/// - **threads_use**: Peak thread count
///
/// # Semantics
///
/// All fields represent **high-water marks** (maximum values) since the last reset:
/// - Values only increase until explicitly reset
/// - Updates ignore values lower than current maximums
/// - Reset clears all values to 0.0
///
/// # Example
///
/// ```rust,ignore
/// let mut metrics = UsageMetrics::new();
/// metrics.update(Some(100.0), Some(50.0), Some(10.0)); // {100, 50, 10}
/// metrics.update(Some(80.0), Some(60.0), Some(5.0));   // {100, 60, 10} (only fd increased)
/// metrics.reset();                                      // {0, 0, 0}
/// ```
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct UsageMetrics {
    /// Peak `/tmp` directory usage in bytes.
    pub tmp_used: f64,

    /// Peak file descriptor count.
    pub fd_use: f64,

    /// Peak thread count.
    pub threads_use: f64,
}

impl UsageMetrics {
    /// Creates a new `UsageMetrics` with all values initialized to 0.0.
    ///
    /// # Returns
    ///
    /// A new metrics instance with zero values.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tmp_used: 0.0,
            fd_use: 0.0,
            threads_use: 0.0,
        }
    }

    /// Updates high-water marks with new measurements (if greater).
    ///
    /// For each provided value, updates the corresponding metric only if the new
    /// value exceeds the current maximum. This implements high-water mark tracking.
    ///
    /// # Arguments
    ///
    /// * `tmp_used` - New `/tmp` usage measurement (bytes), or `None` to skip
    /// * `fd_use` - New file descriptor count, or `None` to skip
    /// * `threads_use` - New thread count, or `None` to skip
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut metrics = UsageMetrics::new();
    /// metrics.update(Some(100.0), None, Some(10.0));  // tmp=100, threads=10
    /// metrics.update(Some(80.0), Some(50.0), None);   // tmp=100 (unchanged), fd=50
    /// ```
    pub fn update(&mut self, tmp_used: Option<f64>, fd_use: Option<f64>, threads_use: Option<f64>) {
        // Update /tmp usage high-water mark
        if let Some(tmp_used) = tmp_used {
            // Decision: Only update if new value exceeds current maximum
            if tmp_used > self.tmp_used {
                self.tmp_used = tmp_used;
            }
        }

        // Update file descriptor high-water mark
        if let Some(fd_use) = fd_use {
            // Decision: Only update if new value exceeds current maximum
            if fd_use > self.fd_use {
                self.fd_use = fd_use;
            }
        }

        // Update thread count high-water mark
        if let Some(threads_use) = threads_use {
            // Decision: Only update if new value exceeds current maximum
            if threads_use > self.threads_use {
                self.threads_use = threads_use;
            }
        }
    }

    /// Resets all high-water marks to 0.0.
    ///
    /// This should be called after flushing metrics to the backend to start
    /// tracking a new flush period.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let metrics = handle.get_metrics().await?;
    /// report_metrics(&metrics);
    /// handle.reset_metrics()?; // Clear for next period
    /// ```
    pub fn reset(&mut self) {
        self.tmp_used = 0.0;
        self.fd_use = 0.0;
        self.threads_use = 0.0;
    }
}

impl Default for UsageMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Commands sent to the enhanced metrics actor.
///
/// This enum defines the message protocol for communicating with the
/// `EnhancedMetricsService` actor. Commands are sent via an unbounded MPSC
/// channel and processed sequentially by the actor.
///
/// # Command Types
///
/// - **Update**: Provide new measurements for high-water mark tracking
/// - **Reset**: Clear all metrics (after flush)
/// - **Get**: Retrieve current metrics (async response via oneshot)
/// - **Shutdown**: Gracefully stop the actor
///
/// # Example
///
/// ```rust,ignore
/// // Update command
/// handle.tx.send(Command::Update(Some(100.0), Some(50.0), Some(10.0)))?;
///
/// // Get command with response channel
/// let (tx, rx) = oneshot::channel();
/// handle.tx.send(Command::Get(tx))?;
/// let metrics = rx.await?;
/// ```
#[derive(Debug)]
pub enum Command {
    /// Update high-water marks with new measurements.
    ///
    /// Arguments: (tmp_used, fd_use, threads_use)
    /// Each `Option<f64>` is `None` to skip that metric, or `Some(value)` to update it.
    Update(Option<f64>, Option<f64>, Option<f64>),

    /// Reset all high-water marks to 0.0.
    ///
    /// Typically called after flushing metrics to start a new tracking period.
    Reset(),

    /// Get current metrics (async response).
    ///
    /// The actor sends the current `UsageMetrics` via the provided oneshot channel.
    Get(oneshot::Sender<UsageMetrics>),

    /// Shutdown the actor gracefully.
    ///
    /// The actor will stop processing commands and exit the `run()` loop.
    Shutdown,
}

/// Handle for sending commands to the enhanced metrics actor.
///
/// This handle provides a safe API for interacting with the `EnhancedMetricsService`
/// actor from multiple threads. All methods use message-passing (no locks).
///
/// # Cloning
///
/// Handles are cheap to clone (internally uses `Arc`). Multiple threads can hold
/// clones and send commands concurrently.
///
/// # Monitoring State
///
/// The handle includes a watch channel for controlling the monitoring loop:
/// - `pause_monitoring()` → Monitoring loop should pause polling
/// - `resume_monitoring()` → Monitoring loop should resume polling
///
/// Note: The actor itself always processes commands; pause/resume only affects
/// the external monitoring loop that sends updates.
#[derive(Clone, Debug)]
pub struct EnhancedMetricsHandle {
    /// Command channel to the actor (unbounded for non-blocking sends).
    tx: mpsc::UnboundedSender<Command>,

    /// Monitoring state broadcast channel (true = active, false = paused).
    monitoring_state_tx: watch::Sender<bool>,
}

impl EnhancedMetricsHandle {
    /// Updates high-water mark metrics with new measurements.
    ///
    /// Sends new measurements to the actor for potential high-water mark updates.
    /// Each metric is only updated if the new value exceeds the current maximum.
    ///
    /// # Arguments
    ///
    /// * `tmp_used` - New `/tmp` usage (bytes), or `None` to skip
    /// * `fd_use` - New file descriptor count, or `None` to skip
    /// * `threads_use` - New thread count, or `None` to skip
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Command sent successfully
    /// * `Err` - Actor has been shut down (channel closed)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// handle.update_metrics(Some(100.0), Some(50.0), Some(10.0))?;
    /// ```
    pub fn update_metrics(
        &self,
        tmp_used: Option<f64>,
        fd_use: Option<f64>,
        threads_use: Option<f64>,
    ) -> Result<(), mpsc::error::SendError<Command>> {
        self.tx.send(Command::Update(tmp_used, fd_use, threads_use))
    }

    /// Resets all high-water marks to 0.0.
    ///
    /// This should be called after flushing metrics to start tracking a new period.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Command sent successfully
    /// * `Err` - Actor has been shut down (channel closed)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let metrics = handle.get_metrics().await?;
    /// report_metrics(&metrics);
    /// handle.reset_metrics()?; // Clear for next period
    /// ```
    pub fn reset_metrics(&self) -> Result<(), mpsc::error::SendError<Command>> {
        self.tx.send(Command::Reset())
    }

    /// Retrieves the current high-water mark metrics (async).
    ///
    /// This sends a `Get` command to the actor and awaits the response via
    /// a oneshot channel.
    ///
    /// # Returns
    ///
    /// * `Ok(metrics)` - Current high-water mark metrics
    /// * `Err` - Actor shut down or response channel closed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let metrics = handle.get_metrics().await?;
    /// println!("Peak /tmp usage: {} bytes", metrics.tmp_used);
    /// ```
    pub async fn get_metrics(&self) -> Result<UsageMetrics, String> {
        // Create oneshot channel for async response
        let (response_tx, response_rx) = oneshot::channel();

        // Send Get command with response channel
        // Decision: Convert SendError to String for consistent error type
        self.tx
            .send(Command::Get(response_tx))
            .map_err(|e| format!("Failed to send enhanced metrics command: {e}"))?;

        // Await response from actor
        // Decision: Convert RecvError to String for consistent error type
        response_rx
            .await
            .map_err(|e| format!("Failed to receive enhanced metrics response: {e}"))
    }

    /// Pauses the monitoring loop (does not affect the actor).
    ///
    /// This broadcasts `false` to all monitoring state receivers, signaling
    /// that monitoring loops should pause polling. The actor itself continues
    /// processing commands.
    ///
    /// # Note
    ///
    /// Errors are silently ignored since pause/resume is advisory.
    pub fn pause_monitoring(&self) {
        // Broadcast pause signal to monitoring loops
        // Decision: Ignore errors since pause/resume is best-effort
        let _ = self
            .monitoring_state_tx
            .send(false)
            .map_err(|e| format!("Failed to pause enhanced metrics monitoring: {e}"));
    }

    /// Resumes the monitoring loop (does not affect the actor).
    ///
    /// This broadcasts `true` to all monitoring state receivers, signaling
    /// that monitoring loops should resume polling.
    ///
    /// # Note
    ///
    /// Errors are silently ignored since pause/resume is advisory.
    pub fn resume_monitoring(&self) {
        // Broadcast resume signal to monitoring loops
        // Decision: Ignore errors since pause/resume is best-effort
        let _ = self
            .monitoring_state_tx
            .send(true)
            .map_err(|e| format!("Failed to resume enhanced metrics monitoring: {e}"));
    }

    /// Gets a receiver for monitoring state changes.
    ///
    /// The receiver provides a stream of state changes:
    /// - `true` - Monitoring should be active (resume polling)
    /// - `false` - Monitoring should be paused (stop polling)
    ///
    /// # Returns
    ///
    /// A `watch::Receiver<bool>` that receives state updates.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut state_rx = handle.get_monitoring_state_receiver();
    ///
    /// loop {
    ///     if *state_rx.borrow() {
    ///         // Monitoring is active - poll resources
    ///         let tmp_used = get_tmp_used().ok();
    ///         handle.update_metrics(tmp_used, None, None)?;
    ///     }
    ///     state_rx.changed().await?; // Wait for state change
    /// }
    /// ```
    #[must_use]
    pub fn get_monitoring_state_receiver(&self) -> watch::Receiver<bool> {
        self.monitoring_state_tx.subscribe()
    }

    /// Gracefully shuts down the actor.
    ///
    /// Sends a `Shutdown` command that causes the actor to exit its `run()` loop.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Shutdown command sent successfully
    /// * `Err` - Actor already shut down (channel closed)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// handle.shutdown()?;
    /// service_task.await?; // Wait for actor to complete
    /// ```
    pub fn shutdown(&self) -> Result<(), mpsc::error::SendError<Command>> {
        self.tx.send(Command::Shutdown)
    }
}

/// Enhanced metrics actor service (owns the metrics state).
///
/// This struct implements the actor pattern for managing high-water mark metrics.
/// It owns the `UsageMetrics` state and processes `Command` messages from the
/// unbounded MPSC channel.
///
/// # Actor Pattern
///
/// - **Single Ownership**: Only this actor mutates the metrics
/// - **Sequential Processing**: Commands processed in FIFO order
/// - **No Locks**: Thread safety via message-passing, not mutexes
/// - **Async**: Integrates with Tokio runtime
///
/// # Lifecycle
///
/// 1. Create service with `new()` → Returns `(service, handle)`
/// 2. Spawn service: `tokio::spawn(service.run())`
/// 3. Use handle to send commands
/// 4. Shutdown: `handle.shutdown()` → Service exits `run()` loop
///
/// # Example
///
/// ```rust,ignore
/// let (service, handle) = EnhancedMetricsService::new();
///
/// // Spawn actor in background
/// let service_task = tokio::spawn(async move {
///     service.run().await;
/// });
///
/// // Use handle to interact
/// handle.update_metrics(Some(100.0), Some(50.0), Some(10.0))?;
/// let metrics = handle.get_metrics().await?;
///
/// // Cleanup
/// handle.shutdown()?;
/// service_task.await?;
/// ```
pub struct EnhancedMetricsService {
    /// High-water mark metrics state (actor's exclusive ownership).
    metrics: UsageMetrics,

    /// Command receiver (unbounded for non-blocking sends).
    rx: mpsc::UnboundedReceiver<Command>,
}

impl EnhancedMetricsService {
    /// Creates a new enhanced metrics service and handle.
    ///
    /// This factory method creates the actor and its corresponding handle,
    /// initializing the MPSC channel and watch channel for communication.
    ///
    /// # Returns
    ///
    /// A tuple of `(service, handle)`:
    /// - **service**: The actor (spawn this with `tokio::spawn`)
    /// - **handle**: Handle for sending commands (clone and share freely)
    ///
    /// # Initial State
    ///
    /// - Metrics: `{tmp_used: 0.0, fd_use: 0.0, threads_use: 0.0}`
    /// - Monitoring state: `false` (paused)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (service, handle) = EnhancedMetricsService::new();
    /// tokio::spawn(async move { service.run().await });
    /// ```
    #[must_use]
    pub fn new() -> (Self, EnhancedMetricsHandle) {
        // Initialize metrics to zero
        let metrics = UsageMetrics::new();

        // Create unbounded MPSC channel for commands
        // Decision: Unbounded to avoid blocking senders (bounded would require backpressure)
        let (tx, rx) = mpsc::unbounded_channel();

        // Create watch channel for monitoring state control
        // Decision: Start paused (false) to avoid unwanted polling before setup complete
        let (monitoring_state_tx, _monitoring_state_rx) = watch::channel(false);

        let service = Self { metrics, rx };
        let handle = EnhancedMetricsHandle {
            tx,
            monitoring_state_tx,
        };

        (service, handle)
    }
}

impl EnhancedMetricsService {
    /// Runs the actor's main event loop (async, blocking).
    ///
    /// This method processes commands from the MPSC receiver until:
    /// - `Command::Shutdown` is received, or
    /// - All handles are dropped (channel closed)
    ///
    /// # Processing Logic
    ///
    /// - **Update**: Updates high-water marks (only if new values exceed current)
    /// - **Reset**: Clears all metrics to 0.0
    /// - **Get**: Sends current metrics via oneshot channel
    /// - **Shutdown**: Logs and exits loop
    ///
    /// # Concurrency
    ///
    /// This method is `!Send` because `self` is consumed. Spawn it with `tokio::spawn`
    /// immediately after creation.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (service, handle) = EnhancedMetricsService::new();
    ///
    /// tokio::spawn(async move {
    ///     service.run().await;
    ///     debug!("Enhanced metrics actor stopped");
    /// });
    /// ```
    pub async fn run(mut self) {
        debug!("Enhanced metrics service started - monitoring usage metrics");

        // Main actor event loop - process commands until shutdown or channel closed
        while let Some(command) = self.rx.recv().await {
            match command {
                Command::Update(tmp_used, fd_use, threads_use) => {
                    // Update high-water marks (only if new values > current)
                    self.metrics.update(tmp_used, fd_use, threads_use);
                }
                Command::Reset() => {
                    // Clear metrics for new tracking period
                    self.metrics.reset();
                }
                Command::Get(response_tx) => {
                    // Send current metrics via oneshot channel
                    // Decision: Log error if receiver dropped (rare but indicates logic bug)
                    if response_tx.send(self.metrics).is_err() {
                        error!("Failed to send enhanced metrics response");
                    }
                }
                Command::Shutdown => {
                    // Graceful shutdown requested
                    debug!("Enhanced metrics service shutting down");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_enhanced_metrics_service() {
        let (service, handle) = EnhancedMetricsService::new();

        let service_handle = tokio::spawn(async move {
            service.run().await;
        });

        let metrics = handle.get_metrics().await.unwrap();
        assert_eq!(metrics, UsageMetrics::new());

        let res = handle.update_metrics(Some(11.0), Some(20.0), Some(5.0));
        assert!(res.is_ok());
        let metrics = handle.get_metrics().await.unwrap();
        assert_eq!(
            metrics,
            UsageMetrics {
                tmp_used: 11.0,
                fd_use: 20.0,
                threads_use: 5.0
            }
        );

        // Test that metrics are only updated if they are greater than the current value
        let res = handle.update_metrics(Some(10.0), Some(10.0), Some(2.0));
        assert!(res.is_ok());
        let metrics = handle.get_metrics().await.unwrap();
        assert_eq!(
            metrics,
            UsageMetrics {
                tmp_used: 11.0,
                fd_use: 20.0,
                threads_use: 5.0
            }
        );

        // Test pause/resume functionality (affects monitoring loop, not service)
        handle.pause_monitoring();
        handle.resume_monitoring();

        // The service itself always processes updates - pause/resume only affects the monitoring loop
        let res = handle.update_metrics(Some(50.0), Some(60.0), Some(70.0));
        assert!(res.is_ok());
        let metrics = handle.get_metrics().await.unwrap();
        assert_eq!(
            metrics,
            UsageMetrics {
                tmp_used: 50.0,
                fd_use: 60.0,
                threads_use: 70.0
            }
        );

        // Test reset metrics
        handle.reset_metrics().unwrap();
        let metrics = handle.get_metrics().await.unwrap();
        assert_eq!(metrics, UsageMetrics::new());

        handle.shutdown().unwrap();
        service_handle.await.unwrap();
    }
}
